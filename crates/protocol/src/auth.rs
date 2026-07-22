use crate::{LocalRequest, ProtocolError};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// HMAC proof stored alongside a local request.  It is intentionally not a
/// bearer token and is valid only for the exact signed request payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthProof {
    pub mac_hex: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallationSecret([u8; 32]);

impl InstallationSecret {
    pub const LEN: usize = 32;

    pub fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }

    pub fn from_hex(value: &str) -> Result<Self, ProtocolError> {
        let bytes = hex::decode(value)
            .map_err(|_| ProtocolError::InvalidInput("installation secret is not hex".into()))?;
        let array: [u8; Self::LEN] = bytes.try_into().map_err(|_| {
            ProtocolError::InvalidInput("installation secret has an invalid length".into())
        })?;
        Ok(Self(array))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn expose_for_storage(&self) -> &[u8; Self::LEN] {
        &self.0
    }

    pub fn sign(&self, request: &LocalRequest) -> Result<AuthProof, ProtocolError> {
        let payload = request.signing_payload();
        let bytes = serde_cbor::to_vec(&payload)?;
        let mut mac =
            HmacSha256::new_from_slice(&self.0).expect("HMAC-SHA256 accepts any key length");
        mac.update(&bytes);
        Ok(AuthProof {
            mac_hex: hex::encode(mac.finalize().into_bytes()),
        })
    }

    pub fn verify(&self, request: &LocalRequest) -> Result<(), ProtocolError> {
        let received = hex::decode(&request.authentication.mac_hex)
            .map_err(|_| ProtocolError::AuthenticationFailed)?;
        let payload = request.signing_payload();
        let bytes = serde_cbor::to_vec(&payload)?;
        let mut mac =
            HmacSha256::new_from_slice(&self.0).expect("HMAC-SHA256 accepts any key length");
        mac.update(&bytes);
        mac.verify_slice(&received)
            .map_err(|_| ProtocolError::AuthenticationFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LocalCommand, LocalRequest, PROTOCOL_VERSION};

    fn request() -> LocalRequest {
        LocalRequest {
            version: PROTOCOL_VERSION,
            request_id: "request-1".into(),
            issued_at_unix_secs: 17,
            command: LocalCommand::ListWorkspaces,
            authentication: AuthProof {
                mac_hex: String::new(),
            },
        }
    }

    #[test]
    fn proof_is_bound_to_every_request_field() {
        let secret = InstallationSecret::from_bytes([7; 32]);
        let mut signed = request();
        signed.authentication = secret.sign(&signed).unwrap();
        assert!(secret.verify(&signed).is_ok());

        signed.issued_at_unix_secs = 18;
        assert!(matches!(
            secret.verify(&signed),
            Err(ProtocolError::AuthenticationFailed)
        ));
    }
}
