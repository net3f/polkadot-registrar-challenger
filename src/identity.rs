use crate::comms::{generate_comms, CommsMain, CommsMessage, CommsVerifier};
use crate::db::Database;
use crate::primitives::{
    Account, AccountType, Algorithm, Challenge, Fatal, NetAccount, NetworkAddress, PubKey, Result,
};
use crossbeam::channel::{unbounded, Receiver, Sender};
use failure::err_msg;
use tokio::time::{self, Duration};

use std::collections::HashMap;
use std::convert::TryInto;

// TODO: add cfg
pub struct TestClient {
    comms: CommsVerifier,
}

impl TestClient {
    pub fn new(comms: CommsVerifier) -> Self {
        TestClient { comms: comms }
    }
    pub fn gen_data(&self) {
        let sk = schnorrkel::keys::SecretKey::generate();
        let pk = sk.to_public();

        let ident = OnChainIdentity {
            network_address: NetworkAddress {
                address: NetAccount::from("test"),
                algo: Algorithm::Schnorr,
                pub_key: PubKey::from(pk),
            },
            display_name: None,
            legal_name: None,
            email: None,
            web: None,
            twitter: None,
            matrix: Some(AccountState::new(
                Account::from("@fabio:web3.foundation"),
                AccountType::Matrix,
            )),
        };

        let random = &ident.matrix.as_ref().unwrap().challenge;

        let sig = sk.sign_simple(b"substrate", &random.as_bytes(), &pk);
        println!("SIG: >> {}", hex::encode(&sig.to_bytes()));

        self.comms.new_on_chain_identity(ident);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OnChainIdentity {
    pub network_address: NetworkAddress,
    pub display_name: Option<String>,
    pub legal_name: Option<String>,
    pub email: Option<AccountState>,
    pub web: Option<AccountState>,
    pub twitter: Option<AccountState>,
    pub matrix: Option<AccountState>,
}

impl OnChainIdentity {
    pub fn pub_key(&self) -> &PubKey {
        &self.network_address.pub_key()
    }
    pub fn from_json(val: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(&val)?)
    }
    pub fn to_json(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountState {
    account: Account,
    account_ty: AccountType,
    account_validity: AccountValidity,
    challenge: Challenge,
    confirmed: bool,
}

impl AccountState {
    pub fn new(account: Account, account_ty: AccountType) -> Self {
        AccountState {
            account: account,
            account_ty: account_ty,
            account_validity: AccountValidity::Unknown,
            challenge: Challenge::gen_random(),
            confirmed: false,
        }
    }
}

#[derive(Eq, PartialEq, Clone, Debug, Serialize, Deserialize)]
enum AccountValidity {
    #[serde(rename = "unknown")]
    Unknown,
    #[serde(rename = "valid")]
    Valid,
    #[serde(rename = "invalid")]
    Invalid,
}

pub struct IdentityManager {
    idents: HashMap<NetAccount, OnChainIdentity>,
    db: Database,
    comms: CommsTable,
}

struct CommsTable {
    to_main: Sender<CommsMessage>,
    listener: Receiver<CommsMessage>,
    pairs: HashMap<AccountType, CommsMain>,
}

impl IdentityManager {
    pub fn new(db: Database) -> Result<Self> {
        let mut idents = HashMap::new();

        // Read pending on-chain identities from storage. Ideally, there are none.
        let db_idents = db.scope("pending_identities");
        for (_, value) in db_idents.all()? {
            let ident = OnChainIdentity::from_json(&*value).fatal();
            idents.insert(ident.network_address.address().clone(), ident);
        }

        let (tx1, recv1) = unbounded();

        Ok(IdentityManager {
            idents: idents,
            db: db,
            comms: CommsTable {
                to_main: tx1.clone(),
                listener: recv1,
                pairs: HashMap::new(),
            },
        })
    }
    pub fn register_comms(&mut self, account_ty: AccountType) -> CommsVerifier {
        let (cm, cv) = generate_comms(self.comms.to_main.clone(), account_ty.clone());
        self.comms.pairs.insert(account_ty, cm);
        cv
    }
    pub async fn start(mut self) -> Result<()> {
        use CommsMessage::*;
        let mut interval = time::interval(Duration::from_millis(50));

        loop {
            if let Ok(msg) = self.comms.listener.try_recv() {
                match msg {
                    CommsMessage::NewOnChainIdentity(ident) => {
                        self.handle_register_request(ident)?;
                    }
                    ValidAccount { network_address: _ } => {}
                    InvalidAccount { network_address: _ } => {}
                    TrackRoomId { address, room_id } => {
                        let db_rooms = self.db.scope("matrix_rooms");
                        db_rooms.put(address.as_str(), room_id.as_bytes())?;
                    }
                    RequestAccountState {
                        account,
                        account_ty,
                    } => {
                        self.handle_account_state_request(account, account_ty);
                    }
                    _ => panic!("Received unrecognized message type. Report as a bug"),
                }
            } else {
                interval.tick().await;
            }
        }
    }
    fn handle_register_request(&mut self, ident: OnChainIdentity) -> Result<()> {
        let db_idents = self.db.scope("pending_identities");
        let db_rooms = self.db.scope("matrix_rooms");

        // Save the pending on-chain identity to disk.
        db_idents.put(ident.network_address.address().as_str(), ident.to_json()?)?;

        // Save the pending on-chain identity to memory.
        self.idents
            .insert(ident.network_address.address().clone(), ident.clone());

        // Only matrix supported for now.
        ident.matrix.as_ref().map::<(), _>(|state| {
            let room_id = if let Some(bytes) = db_rooms.get(&ident.pub_key().to_bytes()).fatal() {
                Some(std::str::from_utf8(&bytes).fatal().try_into().fatal())
            } else {
                None
            };

            self.comms.pairs.get(&state.account_ty).fatal().inform(
                ident.network_address.clone(),
                state.account.clone(),
                state.challenge.clone(),
                room_id,
            );
        });

        Ok(())
    }
    fn handle_account_state_request(&self, account: Account, account_ty: AccountType) {
        let ident = self
            .idents
            .iter()
            .find(|(_, ident)| match account_ty {
                AccountType::Matrix => {
                    if let Some(state) = ident.matrix.as_ref() {
                        state.account == account
                    } else {
                        false
                    }
                }
                _ => panic!("Unsupported"),
            })
            .map(|(_, ident)| ident);

        let comms = self.comms.pairs.get(&AccountType::ReservedEmitter).fatal();

        if let Some(ident) = ident {
            // Account state was checked in `find` combinator.
            let state = match account_ty {
                AccountType::Matrix => ident.matrix.as_ref().fatal(),
                _ => panic!("Unsupported"),
            };

            comms.inform(
                ident.network_address.clone(),
                state.account.clone(),
                state.challenge.clone(),
                None,
            );
        } else {
            // There is no pending request for the user.
            comms.invalid_request();
        }
    }
}
