use crate::aggregate::display_name::DisplayNameHandler;
use crate::event::{
    BlankNetwork, DisplayNamePersisted, FieldStatusVerified, IdentityInserted, Notification,
    RemarkFound,
};
use crate::Result;
use rand::{thread_rng, Rng};
use std::convert::TryFrom;
use std::fmt;
use std::{
    collections::{HashMap, HashSet},
    vec,
};

// TODO: Rename to `ChangeLog`.
pub enum UpdateChanges {
    NewIdentityInserted(NetworkAddress),
    VerificationValid(IdentityField),
    VerificationInvalid(IdentityField),
    BackAndForthExpected(IdentityField),
}

impl From<UpdateChanges> for Notification {
    fn from(val: UpdateChanges) -> Self {
        match val {
            UpdateChanges::NewIdentityInserted(net_address) => Notification::Info(format!(
                "New judgement request for identity {} found.",
                net_address.address_str()
            )),
            UpdateChanges::VerificationValid(field) => {
                Notification::Success(format!("The {} field has been verified.", field))
            }
            UpdateChanges::VerificationInvalid(field) => {
                Notification::Warn(format!("The {} field has failed verification.", field))
            }
            UpdateChanges::BackAndForthExpected(field) => Notification::Info(format!(
                "The first challenge of the {0} field has been verified. \
                An additional challenge has been sent directly to {0}",
                field
            )),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct IdentityManager {
    identities: HashMap<NetworkAddress, HashMap<IdentityFieldType, FieldStatus>>,
    lookup_addresses: HashMap<IdentityField, HashSet<NetworkAddress>>,
    display_names: HashMap<NetworkAddress, DisplayName>,
    on_chain_challenges: HashMap<NetworkAddress, OnChainChallenge>,
}

// TODO: Should logs be printed if users are not found?
impl IdentityManager {
    pub fn export_state(&self) -> Vec<IdentityState> {
        self.identities
            .iter()
            .map(|(net_address, fields)| IdentityState {
                net_address: net_address.clone(),
                on_chain_challenge: self.on_chain_challenges.get(net_address).unwrap().clone(),
                fields: fields.clone(),
            })
            .collect()
    }
    pub fn contains(&self, identity: &IdentityState) -> bool {
        self.identities
            .get(&identity.net_address)
            .map(|state| state == &identity.fields)
            .unwrap_or(false)
    }
    pub fn get_on_chain_challenge(
        &self,
        net_address: &NetworkAddress,
    ) -> Option<&OnChainChallenge> {
        self.on_chain_challenges.get(net_address)
    }
    // TODO: Rename variable to `inserted`
    // TODO: Should return notifications.
    pub fn insert_identity(&mut self, identity: IdentityInserted) {
        // Take value from Event wrapper.
        let identity = identity.identity;

        // Insert identity.
        let (net_address, mut new_fields) = (identity.net_address, identity.fields);
        self.identities
            .entry(net_address.clone())
            .and_modify(|current_fields| {
                // Do a quick check and avoid modifications if the fields match.
                if current_fields == &new_fields {
                    return;
                }

                // Delete all entries which have been removed from the new state.
                current_fields.retain(|field_ty, _current| new_fields.contains_key(field_ty));

                // Retain only entries of which the field address has changed.
                new_fields.retain(|field_ty, field| {
                    current_fields
                        .get(field_ty)
                        .map(|current| current.field != field.field)
                        .unwrap_or(true)
                });

                // Insert all new entries into storage.
                for (field_ty, new) in new_fields.clone() {
                    current_fields.insert(field_ty, new);
                }
            })
            .or_insert(new_fields.clone());

        // Create lookup tables.
        for (_, field) in new_fields {
            self.lookup_addresses
                .entry(field.field.clone())
                .and_modify(|active_addresses| {
                    active_addresses.insert(net_address.clone());
                })
                .or_insert(vec![net_address.clone()].into_iter().collect());
        }

        // Create on-chain challenge.
        self.on_chain_challenges
            .entry(net_address)
            .or_insert(identity.on_chain_challenge.clone());
    }
    // TODO: This should return the full identity, too.
    pub fn update_field(&mut self, verified: FieldStatusVerified) -> Result<Option<UpdateChanges>> {
        self.identities
            .get_mut(&verified.net_address)
            .ok_or(anyhow!("network address not found"))
            .and_then(|statuses| {
                statuses
                    .get_mut(&verified.field_status.field.as_type())
                    .ok_or(anyhow!("field not found"))
                    .map(|current_status| {
                        if let Some(update_changes) =
                            Self::update_changes(current_status, &verified)
                        {
                            // Commit changes.
                            *current_status = verified.field_status;

                            Some(update_changes)
                        } else {
                            None
                        }
                    })
            })
    }
    // TODO: Should return Vec<UpdateChanges>
    // TODO: Should return Result
    fn update_changes(
        current_status: &FieldStatus,
        verified: &FieldStatusVerified,
    ) -> Option<UpdateChanges> {
        let verified_status = &verified.field_status;

        // If the current field status has already been verified, skip, even if
        // the new message is invalid (and avoid sending a new notification).
        if current_status.is_valid() {
            None
        }
        // TODO: Outdated comment:
        // Verification is valid, so commit changes. Generate
        // different notifications based on the individual
        // challenge type.
        else if current_status.is_not_valid() {
            let field = verified_status.field.clone();

            match &verified_status.challenge {
                ChallengeStatus::ExpectMessage(challenge) => {
                    match challenge.status {
                        Validity::Valid => Some(UpdateChanges::VerificationValid(field.clone())),
                        Validity::Invalid => {
                            Some(UpdateChanges::VerificationInvalid(field.clone()))
                        }
                        // TODO: This should technically never occur.
                        Validity::Unconfirmed => None,
                    }
                }
                ChallengeStatus::CheckDisplayName(new_status) => {
                    match new_status.status {
                        Validity::Valid => Some(UpdateChanges::VerificationValid(field.clone())),
                        Validity::Invalid => {
                            Some(UpdateChanges::VerificationInvalid(field.clone()))
                        }
                        // TODO: This should technically never occur.
                        Validity::Unconfirmed => None,
                    }
                }
                ChallengeStatus::BackAndForth(new_challenge_status) => {
                    let curr_challenge_status = match &current_status.challenge {
                        ChallengeStatus::BackAndForth(challenge) => challenge,
                        _ => return None,
                    };

                    // Must verify first challenge, first.
                    if curr_challenge_status.first_check_status != Validity::Valid {
                        match new_challenge_status.first_check_status {
                            Validity::Valid => {
                                Some(UpdateChanges::VerificationValid(field.clone()))
                            }
                            Validity::Invalid => {
                                Some(UpdateChanges::VerificationInvalid(field.clone()))
                            }
                            Validity::Unconfirmed => None,
                        }
                    } else if curr_challenge_status.second_check_status != Validity::Valid {
                        match new_challenge_status.second_check_status {
                            Validity::Valid => {
                                // TODO: This should be returned, too.
                                //Some(UpdateChanges::VerificationValid(field.clone()))
                                Some(UpdateChanges::BackAndForthExpected(field.clone()))
                            }
                            Validity::Invalid => {
                                Some(UpdateChanges::VerificationInvalid(field.clone()))
                            }
                            Validity::Unconfirmed => None,
                        }
                    }
                    // TODO: This case should never occur.
                    else {
                        None
                    }
                }
                ChallengeStatus::Unsupported => {
                    error!("Attempted to get update changes from an unsupported challenge");
                    None
                }
            }
        }
        // This case should never occur. Better safe than sorry.
        else {
            None
        }
    }
    fn lookup_field_status(
        &self,
        net_address: &NetworkAddress,
        field: &IdentityField,
    ) -> Option<&FieldStatus> {
        self.identities
            .get(net_address)
            .and_then(|fields| fields.get(&field.as_type()))
    }
    // Lookup all addresses which contain the specified field.
    fn lookup_addresses(&self, field: &IdentityField) -> Option<Vec<&NetworkAddress>> {
        self.lookup_addresses
            .get(field)
            .map(|addresses| addresses.iter().map(|address| address).collect())
    }
    pub fn lookup_full_state(&self, net_address: &NetworkAddress) -> Option<IdentityState> {
        self.identities
            .get(net_address)
            .map(|fields| IdentityState {
                net_address: net_address.clone(),
                on_chain_challenge: self.on_chain_challenges.get(&net_address).unwrap().clone(),
                fields: fields.clone(),
            })
    }
    pub fn verify_display_name(
        &self,
        net_address: NetworkAddress,
        display_name: DisplayName,
    ) -> Result<Option<VerificationOutcome>> {
        let mut field_status = self
            .lookup_field_status(
                &net_address,
                &IdentityField::DisplayName(display_name.clone()),
            )
            .ok_or(anyhow!(
                "no identity found based on display name: \"{}\", expected: {:?}",
                display_name.as_str(),
                net_address,
            ))?
            .clone();

        let mut challenge = match &field_status.challenge {
            ChallengeStatus::CheckDisplayName(challenge) => {
                if challenge.status == Validity::Valid {
                    // The display name was already verified. Ignore.
                    return Ok(None);
                }

                challenge.clone()
            }
            _ => {
                return Err(anyhow!(
                    "expected to verify display name, found different challenge type"
                ))
            }
        };

        let all_display_names = self.display_names.values().collect::<Vec<&DisplayName>>();
        let handler = DisplayNameHandler::with_state(all_display_names.as_slice());
        let violations = handler.verify_display_name(&display_name);

        let outcome = if violations.is_empty() {
            VerificationOutcome {
                net_address: net_address,
                field_status: {
                    challenge.status = Validity::Valid;
                    challenge.similarities = None;
                    field_status.challenge = ChallengeStatus::CheckDisplayName(challenge);
                    field_status
                },
            }
        } else {
            VerificationOutcome {
                net_address: net_address,
                field_status: {
                    challenge.status = Validity::Invalid;
                    challenge.similarities = Some(violations);
                    field_status.challenge = ChallengeStatus::CheckDisplayName(challenge);
                    field_status
                },
            }
        };

        Ok(Some(outcome))
    }
    pub fn persist_display_name(&mut self, persisted: DisplayNamePersisted) -> Result<()> {
        self.lookup_addresses(&IdentityField::DisplayName(persisted.display_name.clone()))
            .and_then(|addresses| {
                if addresses.contains(&&persisted.net_address) {
                    Some(())
                } else {
                    None
                }
            })
            .ok_or(anyhow!(
                "attempted to persist display name of an identity which does not exist"
            ))?;

        self.display_names
            .insert(persisted.net_address, persisted.display_name);

        Ok(())
    }
    // TODO: This should return `Result<>`
    // TODO: Should set the status to `Invalid` if so.
    pub fn verify_message(
        &self,
        field: &IdentityField,
        provided_message: &ProvidedMessage,
    ) -> Option<VerificationOutcome> {
        // Lookup all addresses which contain the field.
        if let Some(net_addresses) = self.lookup_addresses(field) {
            // For each address, verify the field.
            for net_address in net_addresses {
                if let Some(field_status) = self.lookup_field_status(&net_address, field) {
                    // Variables must be cloned, since those are later converted
                    // into events (which require ownership) and sent to the
                    // event store.
                    let c_net_address = net_address.clone();
                    let mut c_field_status = field_status.clone();

                    // TODO: Make quick `is_valid` check and return if so.

                    // Verify the message, each verified specifically based on
                    // the challenge type.
                    match &field_status.challenge {
                        ChallengeStatus::ExpectMessage(challenge) => {
                            if challenge.status != Validity::Valid {
                                let outcome = if challenge
                                    .expected_message
                                    .contains(&provided_message)
                                    .is_some()
                                {
                                    VerificationOutcome {
                                        net_address: c_net_address,
                                        field_status: {
                                            // Clone the current state and overwrite the validity as `Valid`.
                                            let mut challenge = challenge.clone();
                                            challenge.status = Validity::Valid;
                                            c_field_status.challenge =
                                                ChallengeStatus::ExpectMessage(challenge);
                                            c_field_status
                                        },
                                    }
                                } else {
                                    VerificationOutcome {
                                        net_address: c_net_address,
                                        // Leave current state as is.
                                        field_status: {
                                            // Clone the current state and overwrite the validity as `Invalid`.
                                            let mut challenge = challenge.clone();
                                            challenge.status = Validity::Invalid;
                                            c_field_status.challenge =
                                                ChallengeStatus::ExpectMessage(challenge);
                                            c_field_status
                                        },
                                    }
                                };

                                return Some(outcome);
                            }
                        }
                        ChallengeStatus::BackAndForth(challenge) => {
                            // The first check must be verified before it can
                            // proceed on the seconds check.
                            let outcome = if challenge.first_check_status != Validity::Valid {
                                if challenge
                                    .expected_message
                                    .contains(&provided_message)
                                    .is_some()
                                {
                                    VerificationOutcome {
                                        net_address: c_net_address,
                                        field_status: {
                                            let mut challenge = challenge.clone();
                                            challenge.first_check_status = Validity::Valid;
                                            c_field_status.challenge =
                                                ChallengeStatus::BackAndForth(challenge);
                                            c_field_status
                                        },
                                    }
                                } else {
                                    VerificationOutcome {
                                        net_address: c_net_address,
                                        field_status: {
                                            let mut challenge = challenge.clone();
                                            challenge.first_check_status = Validity::Invalid;
                                            c_field_status.challenge =
                                                ChallengeStatus::BackAndForth(challenge);
                                            c_field_status
                                        },
                                    }
                                }
                            } else if challenge.second_check_status != Validity::Valid {
                                if challenge
                                    .expected_message_back
                                    .contains(&provided_message)
                                    .is_some()
                                {
                                    VerificationOutcome {
                                        net_address: c_net_address,
                                        field_status: {
                                            // Clone the current state and overwrite
                                            // the validity of the **second** status
                                            // as `Valid`.
                                            let mut challenge = challenge.clone();
                                            challenge.second_check_status = Validity::Valid;
                                            c_field_status.challenge =
                                                ChallengeStatus::BackAndForth(challenge);
                                            c_field_status
                                        },
                                    }
                                } else {
                                    VerificationOutcome {
                                        net_address: c_net_address,
                                        field_status: {
                                            // Clone the current state and overwrite
                                            // the validity of the **second** status
                                            // as `Valid`.
                                            let mut challenge = challenge.clone();
                                            challenge.second_check_status = Validity::Invalid;
                                            c_field_status.challenge =
                                                ChallengeStatus::BackAndForth(challenge);
                                            c_field_status
                                        },
                                    }
                                }
                            } else {
                                return None;
                            };

                            return Some(outcome);
                        }
                        ChallengeStatus::CheckDisplayName(_) => {
                            error!("Attempted to verify message of a display name check challenge");
                        }
                        ChallengeStatus::Unsupported => {
                            error!("Attempted to verify message of a unsupported challenge");
                        }
                    }
                }
            }
        };

        None
    }
    pub fn is_fully_verified(&self, net_address: &NetworkAddress) -> Result<bool> {
        self.identities
            .get(net_address)
            .map(|field_statuses| {
                field_statuses.iter().all(|(_, field)| field.is_valid())
            })
            .ok_or(anyhow!(
                "failed to check the full verification status of unknown target: {:?}. This is a bug",
                net_address
            ))
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub struct VerificationOutcome {
    pub net_address: NetworkAddress,
    pub field_status: FieldStatus,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "network", content = "address")]
#[serde(rename_all = "snake_case")]
pub enum NetworkAddress {
    Polkadot(IdentityAddress),
    Kusama(IdentityAddress),
}

impl NetworkAddress {
    pub fn from(network: BlankNetwork, address: IdentityAddress) -> Self {
        match network {
            BlankNetwork::Polkadot => NetworkAddress::Polkadot(address),
            BlankNetwork::Kusama => NetworkAddress::Kusama(address),
        }
    }
    pub fn net_str(&self) -> &str {
        match self {
            NetworkAddress::Polkadot(_) => "polkadot",
            NetworkAddress::Kusama(_) => "kusama",
        }
    }
    pub fn address_str(&self) -> &str {
        match self {
            NetworkAddress::Polkadot(address) => address.0.as_str(),
            NetworkAddress::Kusama(address) => address.0.as_str(),
        }
    }
}

#[derive(Eq, PartialEq, Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct IdentityState {
    pub net_address: NetworkAddress,
    pub on_chain_challenge: OnChainChallenge,
    pub fields: HashMap<IdentityFieldType, FieldStatus>,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct IdentityAddress(String);

impl From<String> for IdentityAddress {
    fn from(val: String) -> Self {
        IdentityAddress(val)
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FieldStatus {
    // TODO: Only make pub for tests.
    pub field: IdentityField,
    is_permitted: bool,
    challenge: ChallengeStatus,
}

impl FieldStatus {
    // TODO: rename to "is_verified"?
    pub fn is_valid(&self) -> bool {
        let status = match &self.challenge {
            ChallengeStatus::ExpectMessage(state) => &state.status,
            ChallengeStatus::BackAndForth(state) => {
                if state.first_check_status == Validity::Valid
                    && state.second_check_status == Validity::Valid
                {
                    return true;
                } else {
                    return false;
                }
            }
            ChallengeStatus::CheckDisplayName(state) => &state.status,
            ChallengeStatus::Unsupported => return false,
        };

        match status {
            Validity::Valid => true,
            Validity::Invalid | Validity::Unconfirmed => false,
        }
    }
    /// Convenience method for improved readability.
    pub fn is_not_valid(&self) -> bool {
        !self.is_valid()
    }
}

impl From<(IdentityField, RegistrarIdentityField)> for FieldStatus {
    fn from(val: (IdentityField, RegistrarIdentityField)) -> Self {
        let field = val.0.clone();
        let challenge = ChallengeStatus::from(val);

        FieldStatus {
            field: field,
            is_permitted: {
                match challenge {
                    ChallengeStatus::Unsupported => false,
                    _ => true,
                }
            },
            challenge: challenge,
        }
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct OnChainChallenge(String);

impl OnChainChallenge {
    pub fn gen() -> Self {
        OnChainChallenge({
            let random: [u8; 16] = thread_rng().gen();
            format!("w3f_registrar:{}", hex::encode(random))
        })
    }
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
    pub fn matches_remark(&self, remark: &RemarkFound) -> bool {
        self.0.as_str() == remark.as_str()
    }
}

// TODO: Maybe rename to `ChallengeType`?
#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "state")]
pub enum ChallengeStatus {
    #[serde(rename = "expect_message")]
    ExpectMessage(ExpectMessageChallenge),
    #[serde(rename = "back_and_forth")]
    BackAndForth(BackAndForthChallenge),
    #[serde(rename = "display_name_check")]
    CheckDisplayName(CheckDisplayNameChallenge),
    #[serde(rename = "unsupported")]
    Unsupported,
}

impl From<(IdentityField, RegistrarIdentityField)> for ChallengeStatus {
    fn from(val: (IdentityField, RegistrarIdentityField)) -> Self {
        let (from, to) = val;

        #[rustfmt::skip]
        let challenge = match &from {
            IdentityField::LegalName(_)
            | IdentityField::PGPFingerprint(_)
            | IdentityField::Web(_)
            | IdentityField::Image
            | IdentityField::Additional => {
                ChallengeStatus::Unsupported
            }
            IdentityField::DisplayName(_) => {
                ChallengeStatus::CheckDisplayName(CheckDisplayNameChallenge {
                    status: Validity::Unconfirmed,
                    similarities: None,
                })
            }
            IdentityField::Email(_) => ChallengeStatus::BackAndForth(BackAndForthChallenge {
                expected_message: ExpectedMessage::gen(),
                expected_message_back: ExpectedMessage::gen(),
                from: from,
                to: to,
                first_check_status: Validity::Unconfirmed,
                second_check_status: Validity::Unconfirmed,
            }),
            IdentityField::Twitter(_) | IdentityField::Matrix(_) => {
                ChallengeStatus::ExpectMessage(ExpectMessageChallenge {
                    expected_message: ExpectedMessage::gen(),
                    from: from,
                    to: to,
                    status: Validity::Unconfirmed,
                })
            }
        };

        challenge
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct ExpectMessageChallenge {
    pub expected_message: ExpectedMessage,
    pub from: IdentityField,
    pub to: RegistrarIdentityField,
    pub status: Validity,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct BackAndForthChallenge {
    pub expected_message: ExpectedMessage,
    // VERY IMPORTANT: This field MUST be skipped during serializing and MAY NO
    // be sent to the the end user via the API, since the message must be
    // explicitly received by the specified `from` address and sent back to the
    // service (`to` address).
    pub expected_message_back: ExpectedMessage,
    pub from: IdentityField,
    pub to: RegistrarIdentityField,
    pub first_check_status: Validity,
    pub second_check_status: Validity,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct CheckDisplayNameChallenge {
    pub status: Validity,
    pub similarities: Option<Vec<DisplayName>>,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
// TODO: Rename to "Verification"?
pub enum Validity {
    #[serde(rename = "valid")]
    Valid,
    #[serde(rename = "invalid")]
    Invalid,
    #[serde(rename = "unconfirmed")]
    Unconfirmed,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct DisplayName(String);

impl DisplayName {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl From<String> for DisplayName {
    fn from(val: String) -> Self {
        DisplayName(val)
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct FieldAddress(String);

impl FieldAddress {
    fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl From<String> for FieldAddress {
    fn from(val: String) -> Self {
        FieldAddress(val)
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct ExpectedMessage(String);

impl ExpectedMessage {
    pub fn gen() -> Self {
        ExpectedMessage({
            let random: [u8; 16] = thread_rng().gen();
            hex::encode(random)
        })
    }
}

// TODO: Should be moved to `crate::events`
#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct ProvidedMessage {
    pub parts: Vec<ProvidedMessagePart>,
}

// TODO: Should be moved to `crate::events`
#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct ProvidedMessagePart(String);

impl From<String> for ProvidedMessagePart {
    fn from(val: String) -> Self {
        ProvidedMessagePart(val)
    }
}

impl ExpectedMessage {
    fn contains<'a>(&self, message: &'a ProvidedMessage) -> Option<&'a ProvidedMessagePart> {
        for part in &message.parts {
            if self.0.contains(&part.0) {
                return Some(part);
            }
        }

        None
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct RegistrarIdentityField {
    field: IdentityField,
}

impl RegistrarIdentityField {
    fn as_type(&self) -> IdentityFieldType {
        self.field.as_type()
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "address")]
pub enum IdentityField {
    #[serde(rename = "legal_name")]
    LegalName(FieldAddress),
    #[serde(rename = "display_name")]
    DisplayName(DisplayName),
    #[serde(rename = "email")]
    Email(FieldAddress),
    #[serde(rename = "web")]
    Web(FieldAddress),
    #[serde(rename = "twitter")]
    Twitter(FieldAddress),
    #[serde(rename = "matrix")]
    Matrix(FieldAddress),
    #[serde(rename = "pgpFingerprint")]
    PGPFingerprint(FieldAddress),
    #[serde(rename = "image")]
    /// NOTE: Currently unsupported.
    Image,
    #[serde(rename = "additional")]
    /// NOTE: Currently unsupported.
    Additional,
}

// TODO: Remove
#[cfg(test)]
impl IdentityField {
    pub fn inner(&self) -> FieldAddress {
        match self {
            IdentityField::Email(addr) => addr.clone(),
            IdentityField::Twitter(addr) => addr.clone(),
            IdentityField::Matrix(addr) => addr.clone(),
            _ => panic!(),
        }
    }
}

impl IdentityField {
    fn as_type(&self) -> IdentityFieldType {
        match self {
            IdentityField::LegalName(_) => IdentityFieldType::LegalName,
            IdentityField::DisplayName(_) => IdentityFieldType::DisplayName,
            IdentityField::Email(_) => IdentityFieldType::Email,
            IdentityField::Web(_) => IdentityFieldType::Web,
            IdentityField::Twitter(_) => IdentityFieldType::Twitter,
            IdentityField::Matrix(_) => IdentityFieldType::Matrix,
            IdentityField::PGPFingerprint(_) => IdentityFieldType::PGPFingerprint,
            IdentityField::Image => IdentityFieldType::Image,
            IdentityField::Additional => IdentityFieldType::Additional,
        }
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityFieldType {
    LegalName,
    DisplayName,
    Email,
    Web,
    Twitter,
    Matrix,
    PGPFingerprint,
    Image,
    Additional,
}

impl fmt::Display for IdentityField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let string = match self {
            IdentityField::LegalName(addr) => format!("legal name (\"{}\")", addr.as_str()),
            IdentityField::DisplayName(addr) => format!("display name (\"{}\")", addr.as_str()),
            IdentityField::Email(addr) => format!("email (\"{}\")", addr.as_str()),
            IdentityField::Web(addr) => format!("web (\"{}\")", addr.as_str()),
            IdentityField::Twitter(addr) => format!("twitter (\"{}\")", addr.as_str()),
            IdentityField::Matrix(addr) => format!("matrix (\"{}\")", addr.as_str()),
            IdentityField::PGPFingerprint(addr) => {
                format!("PGP Fingerprint: (\"{}\")", addr.as_str())
            }
            IdentityField::Image => format!("image"),
            IdentityField::Additional => format!("additional information"),
        };

        write!(f, "{}", string)
    }
}

#[cfg(test)]
/// This module just contains convenient functionality to initialize test data.
/// The actual tests are placed in `src/tests/`.
mod tests {
    use super::*;

    impl From<(NetworkAddress, DisplayName)> for DisplayNamePersisted {
        fn from(val: (NetworkAddress, DisplayName)) -> Self {
            DisplayNamePersisted {
                net_address: val.0,
                display_name: val.1,
            }
        }
    }

    impl FieldStatus {
        pub fn mut_field(&mut self) -> &mut IdentityField {
            &mut self.field
        }
        pub fn challenge(&self) -> &ChallengeStatus {
            &self.challenge
        }
        pub fn challenge_mut(&mut self) -> &mut ChallengeStatus {
            &mut self.challenge
        }
    }

    impl From<ExpectedMessage> for ProvidedMessage {
        fn from(val: ExpectedMessage) -> Self {
            ProvidedMessage {
                parts: vec![ProvidedMessagePart::from(val.0)],
            }
        }
    }

    impl ExpectedMessage {
        pub fn invalid() -> Self {
            ExpectedMessage("invalid_message".to_string())
        }
    }

    impl NetworkAddress {
        pub fn alice() -> Self {
            NetworkAddress::Polkadot(IdentityAddress::from(
                "1gfpAmeKYhEoSrEgQ5UDYTiNSeKPvxVfLVWcW73JGnX9L6M".to_string(),
            ))
        }
        pub fn bob() -> Self {
            NetworkAddress::Polkadot(IdentityAddress::from(
                "15iMSee2Zg3kJBu3HjimR5zVLNdNHvpUeWwrp4iAL4x7KZ8P".to_string(),
            ))
        }
        pub fn eve() -> Self {
            NetworkAddress::Polkadot(IdentityAddress::from(
                "12sgvwDcEenDwAppRquN8Yh6Bu4um5x2PRyURLwP42XVMg45".to_string(),
            ))
        }
    }

    impl IdentityState {
        pub fn alice() -> Self {
            IdentityState {
                net_address: NetworkAddress::alice(),
                on_chain_challenge: OnChainChallenge::gen(),
                fields: vec![
                    FieldStatus::from({
                        (
                            IdentityField::DisplayName(DisplayName::from("Alice")),
                            RegistrarIdentityField::display_name(),
                        )
                    }),
                    FieldStatus::from({
                        (
                            IdentityField::Email(FieldAddress::from("alice@email.com".to_string())),
                            RegistrarIdentityField::email(),
                        )
                    }),
                    FieldStatus::from({
                        (
                            IdentityField::Twitter(FieldAddress::from("@alice".to_string())),
                            RegistrarIdentityField::twitter(),
                        )
                    }),
                    FieldStatus::from({
                        (
                            IdentityField::Matrix(FieldAddress::from(
                                "@alice:matrix.org".to_string(),
                            )),
                            RegistrarIdentityField::matrix(),
                        )
                    }),
                ]
                .into_iter()
                .map(|field| (field.field.as_type(), field))
                .collect(),
            }
        }
        pub fn bob() -> Self {
            IdentityState {
                net_address: NetworkAddress::bob(),
                on_chain_challenge: OnChainChallenge::gen(),
                fields: vec![
                    FieldStatus::from({
                        (
                            IdentityField::DisplayName(DisplayName::from("Bob")),
                            RegistrarIdentityField::display_name(),
                        )
                    }),
                    FieldStatus::from({
                        (
                            IdentityField::Email(FieldAddress::from("bob@email.com".to_string())),
                            RegistrarIdentityField::email(),
                        )
                    }),
                    FieldStatus::from({
                        (
                            IdentityField::Twitter(FieldAddress::from("@bob".to_string())),
                            RegistrarIdentityField::twitter(),
                        )
                    }),
                    FieldStatus::from({
                        (
                            IdentityField::Matrix(FieldAddress::from(
                                "@bob:matrix.org".to_string(),
                            )),
                            RegistrarIdentityField::matrix(),
                        )
                    }),
                ]
                .into_iter()
                .map(|field| (field.field.as_type(), field))
                .collect(),
            }
        }
        pub fn eve() -> Self {
            IdentityState {
                net_address: NetworkAddress::eve(),
                on_chain_challenge: OnChainChallenge::gen(),
                fields: vec![
                    FieldStatus::from({
                        (
                            IdentityField::DisplayName(DisplayName::from("Eve")),
                            RegistrarIdentityField::display_name(),
                        )
                    }),
                    FieldStatus::from({
                        (
                            IdentityField::Email(FieldAddress::from("eve@email.com".to_string())),
                            RegistrarIdentityField::email(),
                        )
                    }),
                    FieldStatus::from({
                        (
                            IdentityField::Twitter(FieldAddress::from("@eve".to_string())),
                            RegistrarIdentityField::twitter(),
                        )
                    }),
                    FieldStatus::from({
                        (
                            IdentityField::Matrix(FieldAddress::from(
                                "@eve:matrix.org".to_string(),
                            )),
                            RegistrarIdentityField::matrix(),
                        )
                    }),
                ]
                .into_iter()
                .map(|field| (field.field.as_type(), field))
                .collect(),
            }
        }
    }

    impl RegistrarIdentityField {
        // The value itself is not relevant, this method is just used as a
        // throw-away value in the `IdentityState` test implementations above.
        pub fn display_name() -> Self {
            RegistrarIdentityField {
                field: IdentityField::DisplayName(DisplayName::from("Registrar")),
            }
        }
        pub fn email() -> Self {
            RegistrarIdentityField {
                field: IdentityField::Email(FieldAddress::from(
                    "registrar@web3.foundation".to_string(),
                )),
            }
        }
        pub fn twitter() -> Self {
            RegistrarIdentityField {
                field: IdentityField::Twitter(FieldAddress::from("@w3f_registrar".to_string())),
            }
        }
        pub fn matrix() -> Self {
            RegistrarIdentityField {
                field: IdentityField::Matrix(FieldAddress::from(
                    "@registrar:web3.foundation".to_string(),
                )),
            }
        }
    }
}
