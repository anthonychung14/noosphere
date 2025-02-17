use crate::authority::{generate_capability, SphereAction, SPHERE_SEMANTICS, SUPPORTED_KEYS};
use anyhow::Result;
use cid::Cid;
use libipld_cbor::DagCborCodec;
use noosphere_storage::BlockStore;
use serde::{de, ser, Deserialize, Serialize};
use std::{convert::TryFrom, fmt::Display, ops::Deref, str::FromStr};
use ucan::{chain::ProofChain, crypto::did::DidParser, store::UcanJwtStore, Ucan};

use super::{Did, IdentitiesIpld, Jwt, Link};

#[cfg(docs)]
use crate::data::SphereIpld;

/// A subdomain of a [SphereIpld] that pertains to the management and recording of
/// the petnames associated with the sphere.
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize, Hash)]
pub struct AddressBookIpld {
    pub identities: Link<IdentitiesIpld>,
}

impl AddressBookIpld {
    /// Initialize an empty [AddressBookIpld], with a valid [Cid] that refers to
    /// an empty [IdentitiesIpld] in the provided storage
    pub async fn empty<S: BlockStore>(store: &mut S) -> Result<Self> {
        let identities_ipld = IdentitiesIpld::empty(store).await?;
        let identities = store.save::<DagCborCodec, _>(identities_ipld).await?.into();

        Ok(AddressBookIpld { identities })
    }
}

/// An [IdentityIpld] represents an entry in a user's pet name address book.
/// It is intended to be associated with a human readable name, and enables the
/// user to resolve the name to a DID. Eventually the DID will be resolved by
/// some mechanism to a UCAN, so this struct also records the last resolved
/// value if one has ever been resolved.
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize, Hash)]
pub struct IdentityIpld {
    pub did: Did,
    pub link_record: Option<Link<LinkRecord>>,
}

impl IdentityIpld {
    /// If there is a [LinkRecord] for this [IdentityIpld], attempt to retrieve
    /// it from storage
    pub async fn link_record<S: UcanJwtStore>(&self, store: &S) -> Option<LinkRecord> {
        match &self.link_record {
            Some(cid) => match store.read_token(cid).await.unwrap_or(None) {
                Some(jwt) => LinkRecord::from_str(&jwt).ok(),
                None => None,
            },
            _ => None,
        }
    }
}

/// A [LinkRecord] is a wrapper around a decoded [Jwt] ([Ucan]),
/// representing a link address as a [Cid] to a sphere.
#[derive(Debug, Clone)]
#[repr(transparent)]
pub struct LinkRecord(Ucan);

impl LinkRecord {
    /// Validates the [Ucan] token as a [LinkRecord], ensuring that
    /// the sphere's owner authorized the publishing of a new
    /// content address. Notably does not check the publishing timeframe
    /// permissions, as an expired token can be considered valid.
    /// Returns an `Err` if validation fails.
    pub async fn validate<S: UcanJwtStore>(&self, store: &S) -> Result<()> {
        let identity = self.sphere_identity();
        let token = &self.0;

        if self.get_link().is_none() {
            return Err(anyhow::anyhow!("LinkRecord missing link."));
        }

        let mut did_parser = DidParser::new(SUPPORTED_KEYS);

        // We're interested in the validity of the proof at the time
        // of publishing.
        let now_time = if let Some(nbf) = token.not_before() {
            nbf.to_owned()
        } else {
            token.expires_at() - 1
        };

        let proof =
            ProofChain::from_ucan(token.to_owned(), Some(now_time), &mut did_parser, store).await?;

        {
            let desired_capability = generate_capability(identity, SphereAction::Publish);
            let mut has_capability = false;
            for capability_info in proof.reduce_capabilities(&SPHERE_SEMANTICS) {
                let capability = capability_info.capability;
                if capability_info.originators.contains(identity)
                    && capability.enables(&desired_capability)
                {
                    has_capability = true;
                    break;
                }
            }
            if !has_capability {
                return Err(anyhow::anyhow!("LinkRecord is not authorized."));
            }
        }

        token
            .check_signature(&mut did_parser)
            .await
            .map(|_| ())
            .map_err(|_| anyhow::anyhow!("LinkRecord has invalid signature."))
    }

    /// Returns true if the [Ucan] token is currently publishable
    /// within the bounds of its expiry/not before time.
    pub fn has_publishable_timeframe(&self) -> bool {
        !self.0.is_expired(None) && !self.0.is_too_early()
    }

    /// The DID key of the sphere that this record maps.
    pub fn sphere_identity(&self) -> &str {
        self.0.audience()
    }

    /// The sphere revision address ([Cid]) that the sphere's identity maps to.
    pub fn get_link(&self) -> Option<Cid> {
        let facts = self.0.facts();

        for fact in facts {
            match fact.as_object() {
                Some(fields) => match fields.get("link") {
                    Some(cid_string) => {
                        match Cid::try_from(cid_string.as_str().unwrap_or_default()) {
                            Ok(cid) => return Some(cid),
                            Err(error) => {
                                warn!(
                                    "Could not parse '{}' as name record link: {}",
                                    cid_string, error
                                );
                                continue;
                            }
                        }
                    }
                    None => {
                        warn!("No 'link' field in fact, skipping...");
                        continue;
                    }
                },
                None => {
                    warn!("Fact is not an object, skipping...");
                    continue;
                }
            }
        }
        warn!("No facts contained a link!");
        None
    }
}

impl ser::Serialize for LinkRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        let encoded = self.encode().map_err(ser::Error::custom)?;
        serializer.serialize_str(&encoded)
    }
}

impl<'de> de::Deserialize<'de> for LinkRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let record = LinkRecord::try_from(s).map_err(de::Error::custom)?;
        Ok(record)
    }
}

/// [LinkRecord]s compare their [Jwt] representations
/// for equality. If a record cannot be encoded as such,
/// they will not be considered equal to any other record.
impl PartialEq for LinkRecord {
    fn eq(&self, other: &Self) -> bool {
        if let Ok(encoded_a) = self.encode() {
            if let Ok(encoded_b) = other.encode() {
                return encoded_a == encoded_b;
            }
        }
        false
    }
}
impl Eq for LinkRecord {}

impl Deref for LinkRecord {
    type Target = Ucan;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Display for LinkRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "LinkRecord({}, {})",
            self.sphere_identity(),
            self.get_link()
                .map_or_else(|| String::from("None"), String::from)
        )
    }
}

impl TryFrom<&Jwt> for LinkRecord {
    type Error = anyhow::Error;
    fn try_from(value: &Jwt) -> Result<Self, Self::Error> {
        LinkRecord::from_str(value)
    }
}

impl TryFrom<&LinkRecord> for Jwt {
    type Error = anyhow::Error;
    fn try_from(value: &LinkRecord) -> Result<Self, Self::Error> {
        Ok(Jwt(value.encode()?))
    }
}

impl TryFrom<Jwt> for LinkRecord {
    type Error = anyhow::Error;
    fn try_from(value: Jwt) -> Result<Self, Self::Error> {
        LinkRecord::try_from(&value)
    }
}

impl TryFrom<LinkRecord> for Jwt {
    type Error = anyhow::Error;
    fn try_from(value: LinkRecord) -> Result<Self, Self::Error> {
        Jwt::try_from(&value)
    }
}

impl From<&Ucan> for LinkRecord {
    fn from(value: &Ucan) -> Self {
        LinkRecord::from(value.to_owned())
    }
}

impl From<&LinkRecord> for Ucan {
    fn from(value: &LinkRecord) -> Self {
        value.0.clone()
    }
}

impl From<Ucan> for LinkRecord {
    fn from(value: Ucan) -> Self {
        LinkRecord(value)
    }
}

impl From<LinkRecord> for Ucan {
    fn from(value: LinkRecord) -> Self {
        value.0
    }
}

impl TryFrom<&[u8]> for LinkRecord {
    type Error = anyhow::Error;
    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        LinkRecord::try_from(value.to_vec())
    }
}

impl TryFrom<Vec<u8>> for LinkRecord {
    type Error = anyhow::Error;
    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        LinkRecord::from_str(&String::from_utf8(value)?)
    }
}

impl TryFrom<LinkRecord> for Vec<u8> {
    type Error = anyhow::Error;
    fn try_from(value: LinkRecord) -> Result<Self, Self::Error> {
        Ok(value.encode()?.into_bytes())
    }
}

impl FromStr for LinkRecord {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Ucan::from_str(value)?.into())
    }
}

impl TryFrom<String> for LinkRecord {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Ok(Ucan::from_str(&value)?.into())
    }
}

#[cfg(test)]
#[cfg(test)]
mod test {
    use super::*;
    use crate::{authority::generate_ed25519_key, data::Did, view::SPHERE_LIFETIME};
    use noosphere_storage::{MemoryStorage, SphereDb};
    use serde_json::json;
    use ucan::{builder::UcanBuilder, crypto::KeyMaterial, store::UcanJwtStore};

    pub async fn from_issuer<K: KeyMaterial>(
        issuer: &K,
        sphere_id: &Did,
        link: &Cid,
        proofs: Option<&Vec<Ucan>>,
    ) -> Result<LinkRecord, anyhow::Error> {
        let capability = generate_capability(sphere_id, SphereAction::Publish);
        let fact = json!({ "link": link.to_string() });

        let mut builder = UcanBuilder::default()
            .issued_by(issuer)
            .for_audience(sphere_id)
            .claiming_capability(&capability)
            .with_fact(fact);

        if let Some(proofs) = proofs {
            let mut earliest_expiry: u64 = u64::MAX;
            for token in proofs {
                earliest_expiry = *token.expires_at().min(&earliest_expiry);
                builder = builder.witnessed_by(token);
            }
            builder = builder.with_expiration(earliest_expiry);
        } else {
            builder = builder.with_lifetime(SPHERE_LIFETIME);
        }

        Ok(builder.build()?.sign().await?.into())
    }

    async fn expect_failure(message: &str, store: &SphereDb<MemoryStorage>, record: LinkRecord) {
        assert!(record.validate(store).await.is_err(), "{}", message);
    }

    #[tokio::test]
    async fn test_self_signed_link_record() -> Result<(), anyhow::Error> {
        let sphere_key = generate_ed25519_key();
        let sphere_identity = Did::from(sphere_key.get_did().await?);
        let link = "bafyr4iagi6t6khdrtbhmyjpjgvdlwv6pzylxhuhstxhkdp52rju7er325i";
        let cid_link: Cid = link.parse()?;
        let store = SphereDb::new(&MemoryStorage::default()).await.unwrap();

        let record = from_issuer(&sphere_key, &sphere_identity, &cid_link, None).await?;

        assert_eq!(&Did::from(record.sphere_identity()), &sphere_identity);
        assert_eq!(LinkRecord::get_link(&record), Some(cid_link));
        LinkRecord::validate(&record, &store).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_delegated_link_record() -> Result<(), anyhow::Error> {
        let owner_key = generate_ed25519_key();
        let owner_identity = Did::from(owner_key.get_did().await?);
        let sphere_key = generate_ed25519_key();
        let sphere_identity = Did::from(sphere_key.get_did().await?);
        let link = "bafyr4iagi6t6khdrtbhmyjpjgvdlwv6pzylxhuhstxhkdp52rju7er325i";
        let cid_link: Cid = link.parse()?;
        let mut store = SphereDb::new(&MemoryStorage::default()).await.unwrap();

        // First verify that `owner` cannot publish for `sphere`
        // without delegation.
        let record = from_issuer(&owner_key, &sphere_identity, &cid_link, None).await?;

        assert_eq!(record.sphere_identity(), &sphere_identity);
        assert_eq!(record.get_link(), Some(cid_link.clone()));
        if LinkRecord::validate(&record, &store).await.is_ok() {
            panic!("Owner should not have authorization to publish record")
        }

        // Delegate `sphere_key`'s publishing authority to `owner_key`
        let delegate_ucan = UcanBuilder::default()
            .issued_by(&sphere_key)
            .for_audience(&owner_identity)
            .with_lifetime(SPHERE_LIFETIME)
            .claiming_capability(&generate_capability(
                &sphere_identity,
                SphereAction::Publish,
            ))
            .build()?
            .sign()
            .await?;
        let _ = store.write_token(&delegate_ucan.encode()?).await?;

        // Attempt `owner` publishing `sphere` with the proper authorization.
        let proofs = vec![delegate_ucan.clone()];
        let record = from_issuer(&owner_key, &sphere_identity, &cid_link, Some(&proofs)).await?;

        assert_eq!(record.sphere_identity(), &sphere_identity);
        assert_eq!(record.get_link(), Some(cid_link.clone()));
        assert!(LinkRecord::has_publishable_timeframe(&record));
        LinkRecord::validate(&record, &store).await?;

        // Now test a similar record that has an expired capability.
        // It must still be valid.
        let expired: LinkRecord = UcanBuilder::default()
            .issued_by(&owner_key)
            .for_audience(&sphere_identity)
            .claiming_capability(&generate_capability(
                &sphere_identity,
                SphereAction::Publish,
            ))
            .with_fact(json!({ "link": &cid_link.to_string() }))
            .witnessed_by(&delegate_ucan)
            .with_expiration(ucan::time::now() - 1234)
            .build()?
            .sign()
            .await?
            .into();
        assert_eq!(expired.sphere_identity(), &sphere_identity);
        assert_eq!(expired.get_link(), Some(cid_link));
        assert!(expired.has_publishable_timeframe() == false);
        LinkRecord::validate(&record, &store).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_link_record_failures() -> Result<(), anyhow::Error> {
        let sphere_key = generate_ed25519_key();
        let sphere_identity = Did::from(sphere_key.get_did().await?);
        let cid_address = "bafyr4iagi6t6khdrtbhmyjpjgvdlwv6pzylxhuhstxhkdp52rju7er325i";
        let store = SphereDb::new(&MemoryStorage::default()).await.unwrap();

        expect_failure(
            "fails when expect `fact` is missing",
            &store,
            UcanBuilder::default()
                .issued_by(&sphere_key)
                .for_audience(&sphere_identity)
                .with_lifetime(1000)
                .claiming_capability(&generate_capability(
                    &sphere_identity,
                    SphereAction::Publish,
                ))
                .with_fact(json!({ "invalid_fact": cid_address }))
                .build()?
                .sign()
                .await?
                .into(),
        )
        .await;

        let capability = generate_capability(
            &Did(generate_ed25519_key().get_did().await?),
            SphereAction::Publish,
        );
        expect_failure(
            "fails when capability resource does not match sphere identity",
            &store,
            UcanBuilder::default()
                .issued_by(&sphere_key)
                .for_audience(&sphere_identity)
                .with_lifetime(1000)
                .claiming_capability(&capability)
                .with_fact(json!({ "link": cid_address.clone() }))
                .build()?
                .sign()
                .await?
                .into(),
        )
        .await;

        let non_auth_key = generate_ed25519_key();
        expect_failure(
            "fails when a non-authorized key signs the record",
            &store,
            UcanBuilder::default()
                .issued_by(&non_auth_key)
                .for_audience(&sphere_identity)
                .with_lifetime(1000)
                .claiming_capability(&generate_capability(
                    &sphere_identity,
                    SphereAction::Publish,
                ))
                .with_fact(json!({ "link": cid_address.clone() }))
                .build()?
                .sign()
                .await?
                .into(),
        )
        .await;

        Ok(())
    }

    #[tokio::test]
    async fn test_link_record_convert() -> Result<(), anyhow::Error> {
        let sphere_key = generate_ed25519_key();
        let identity = Did::from(sphere_key.get_did().await?);
        let capability = generate_capability(&identity, SphereAction::Publish);
        let cid_address = "bafyr4iagi6t6khdrtbhmyjpjgvdlwv6pzylxhuhstxhkdp52rju7er325i";
        let link = Cid::from_str(cid_address)?;
        let maybe_link = Some(link.clone());
        let fact = json!({ "link": cid_address });

        let ucan = UcanBuilder::default()
            .issued_by(&sphere_key)
            .for_audience(&identity)
            .with_lifetime(1000)
            .claiming_capability(&capability)
            .with_fact(fact)
            .build()?
            .sign()
            .await?;

        let encoded = ucan.encode()?;
        let base = LinkRecord::from(ucan.clone());

        // from_str, String
        {
            let record: LinkRecord = encoded.parse()?;
            assert_eq!(record.sphere_identity(), identity, "LinkRecord::from_str()");
            assert_eq!(record.get_link(), maybe_link, "LinkRecord::from_str()");
            let record: LinkRecord = String::from(encoded.clone()).try_into()?;
            assert_eq!(
                record.sphere_identity(),
                identity,
                "LinkRecord::try_from(String)"
            );
            assert_eq!(
                record.get_link(),
                maybe_link,
                "LinkRecord::try_from(String)"
            );
        }

        // Ucan convert
        {
            let from_ucan_ref = LinkRecord::from(&ucan);
            assert_eq!(base.sphere_identity(), identity, "LinkRecord::from(Ucan)");
            assert_eq!(base.get_link(), maybe_link, "LinkRecord::from(Ucan)");
            assert_eq!(
                from_ucan_ref.sphere_identity(),
                identity,
                "LinkRecord::from(&Ucan)"
            );
            assert_eq!(
                from_ucan_ref.get_link(),
                maybe_link,
                "LinkRecord::from(&Ucan)"
            );
            assert_eq!(
                Ucan::from(base.clone()).encode()?,
                encoded,
                "Ucan::from(LinkRecord)"
            );
            assert_eq!(
                Ucan::from(&base).encode()?,
                encoded,
                "Ucan::from(&LinkRecord)"
            );
        };

        // Vec<u8> convert
        {
            let bytes = Vec::from(encoded.clone());
            let record = LinkRecord::try_from(bytes.clone())?;
            assert_eq!(
                record.sphere_identity(),
                identity,
                "LinkRecord::try_from(Vec<u8>)"
            );
            assert_eq!(
                record.get_link(),
                maybe_link,
                "LinkRecord::try_from(Vec<u8>)"
            );

            let record = LinkRecord::try_from(bytes.as_slice())?;
            assert_eq!(
                record.sphere_identity(),
                identity,
                "LinkRecord::try_from(&[u8])"
            );
            assert_eq!(record.get_link(), maybe_link, "LinkRecord::try_from(&[u8])");

            let bytes_from_record: Vec<u8> = record.try_into()?;
            assert_eq!(bytes_from_record, bytes, "LinkRecord::try_into(Vec<u8>>)");
        };

        // LinkRecord::serialize
        // LinkRecord::deserialize
        {
            let serialized = serde_json::to_string(&base)?;
            assert_eq!(serialized, format!("\"{}\"", encoded), "serialize()");
            let record: LinkRecord = serde_json::from_str(&serialized)?;
            assert_eq!(record.sphere_identity(), identity, "deserialize()");
            assert_eq!(record.get_link(), maybe_link, "deserialize()");
        }

        Ok(())
    }
}
