use crate::try_or_reset::TryOrReset;
use anyhow::anyhow;
use anyhow::Result;
use cid::Cid;
use noosphere_core::data::{ContentType, Did, IdentityIpld, LinkRecord, MapOperation};
use noosphere_ipfs::{IpfsStore, KuboClient};
use noosphere_ns::{server::HttpClient as NameSystemHttpClient, NameResolver};
use noosphere_sphere::{
    HasMutableSphereContext, SphereCursor, SpherePetnameRead, SpherePetnameWrite,
};
use noosphere_sphere::{SphereContentRead, SphereContentWrite, COUNTERPART};
use noosphere_storage::KeyValueStore;
use noosphere_storage::{BlockStoreRetry, Storage, UcanStore};
use std::fmt::Display;
use std::future::Future;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    string::ToString,
    sync::Arc,
    time::Duration,
};
use strum_macros::Display;
use tokio::io::AsyncReadExt;
use tokio::{
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        oneshot::Sender,
    },
    task::JoinHandle,
};
use tokio_stream::{Stream, StreamExt};
use ucan::crypto::KeyMaterial;
use url::Url;

const PERIODIC_PUBLISH_INTERVAL_SECONDS: u64 = 5 * 60;
/// How many seconds between queueing up an address
/// to resolve from the name system.
const PERIODIC_RESOLVER_INTERVAL_SECONDS: u64 = 60;

pub struct NameSystemConfiguration {
    pub connection_type: NameSystemConnectionType,
    pub ipfs_api: Url,
}

impl Display for NameSystemConfiguration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "NameSystemConfiguration({}, {})",
            self.connection_type, self.ipfs_api
        )
    }
}

#[derive(Clone)]
pub enum NameSystemConnectionType {
    Remote(Url),
    // TODO(#255): Configuration for self-managed node
    //InProcess(...)
}

impl Display for NameSystemConnectionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameSystemConnectionType::Remote(url) => Display::fmt(url, f),
        }
    }
}

#[derive(Display)]
pub enum NameSystemJob<C> {
    /// Resolve all names in the sphere at the latest version
    ResolveAll { context: C },
    /// Resolve a single name from a given sphere at the latest version
    #[allow(dead_code)]
    ResolveImmediately {
        context: C,
        name: String,
        tx: Sender<Option<Cid>>,
    },
    /// Resolve all added names of a given sphere since the given sphere
    /// revision
    ResolveSince { context: C, since: Option<Cid> },
    /// Publish a link record (given as a [Jwt]) to the name system
    Publish {
        context: C,
        record: LinkRecord,
        temporary_validate_expiry: bool,
    },
}

pub fn start_name_system<C, K, S>(
    configuration: NameSystemConfiguration,
    local_spheres: Vec<C>,
) -> (UnboundedSender<NameSystemJob<C>>, JoinHandle<Result<()>>)
where
    C: HasMutableSphereContext<K, S> + 'static,
    K: KeyMaterial + Clone + 'static,
    S: Storage + 'static,
{
    let (tx, rx) = unbounded_channel();

    let task = {
        let tx = tx.clone();
        tokio::task::spawn(async move {
            let _ = tokio::join!(
                periodic_publisher_task(tx.clone(), local_spheres.clone()),
                name_system_task(configuration, rx),
                periodic_resolver_task(tx, local_spheres)
            );
            Ok(())
        })
    };

    (tx, task)
}

/// Run once on gateway start and every PERIODIC_PUBLISH_INTERVAL_SECONDS,
/// republish all stored link records in gateway spheres that map to
/// counterpart managed spheres.
async fn periodic_publisher_task<C, K, S>(
    tx: UnboundedSender<NameSystemJob<C>>,
    local_spheres: Vec<C>,
) where
    C: HasMutableSphereContext<K, S>,
    K: KeyMaterial + Clone + 'static,
    S: Storage + 'static,
{
    loop {
        for local_sphere in local_spheres.iter() {
            if let Err(error) = periodic_publish_record(&tx, local_sphere).await {
                error!("Could not publish record: {}", error);
            };
        }
        tokio::time::sleep(Duration::from_secs(PERIODIC_PUBLISH_INTERVAL_SECONDS)).await;
    }
}

async fn periodic_publish_record<C, K, S>(
    tx: &UnboundedSender<NameSystemJob<C>>,
    local_sphere: &C,
) -> Result<()>
where
    C: HasMutableSphereContext<K, S>,
    K: KeyMaterial + Clone + 'static,
    S: Storage + 'static,
{
    match get_counterpart_record(local_sphere).await {
        Ok(Some(record)) => {
            debug!("Got counterpart record.");
            if let Err(error) = tx.send(NameSystemJob::Publish {
                context: local_sphere.to_owned(),
                record,
                temporary_validate_expiry: false,
            }) {
                warn!("Failed to request name record publish: {}", error);
            }
        }
        _ => {
            warn!("Could not find most recent record for counterpart sphere to publish.");
        }
    }
    Ok(())
}

async fn periodic_resolver_task<C, K, S>(
    tx: UnboundedSender<NameSystemJob<C>>,
    local_spheres: Vec<C>,
) where
    C: HasMutableSphereContext<K, S>,
    K: KeyMaterial + Clone + 'static,
    S: Storage + 'static,
{
    for sphere in local_spheres.iter().cycle() {
        match tx.send(NameSystemJob::ResolveAll {
            context: sphere.clone(),
        }) {
            Ok(_) => (),
            Err(error) => {
                warn!("Failed to request updated name resolutions: {}", error);
            }
        }

        tokio::time::sleep(Duration::from_secs(PERIODIC_RESOLVER_INTERVAL_SECONDS)).await;
    }
}

async fn name_system_task<C, K, S>(
    configuration: NameSystemConfiguration,
    mut receiver: UnboundedReceiver<NameSystemJob<C>>,
) -> Result<()>
where
    C: HasMutableSphereContext<K, S>,
    K: KeyMaterial + Clone + 'static,
    S: Storage + 'static,
{
    info!(
        "Resolving from and publishing to Noosphere Name System at {}",
        configuration
    );

    let mut with_client = TryOrReset::new(|| async {
        match &configuration.connection_type {
            NameSystemConnectionType::Remote(url) => {
                NameSystemHttpClient::new(url.to_owned()).await
            }
        }
    });

    let ipfs_api = configuration.ipfs_api.clone();
    while let Some(job) = receiver.recv().await {
        if let Err(error) = process_job(job, &mut with_client, &ipfs_api).await {
            warn!("Error processing NS job: {}", error);
        }
    }
    Ok(())
}

async fn process_job<C, K, S, I, O, F>(
    job: NameSystemJob<C>,
    with_client: &mut TryOrReset<I, O, F>,
    ipfs_api: &Url,
) -> Result<()>
where
    C: HasMutableSphereContext<K, S>,
    K: KeyMaterial + Clone + 'static,
    S: Storage + 'static,
    I: Fn() -> F,
    O: NameResolver + 'static,
    F: Future<Output = Result<O, anyhow::Error>>,
{
    let run_job = with_client.invoke(|client| async move {
        debug!("Running {}", job);
        match job {
            NameSystemJob::Publish {
                record,
                context,
                temporary_validate_expiry,
            } => {
                if let Err(error) = set_counterpart_record(context, &record).await {
                    warn!("Could not set counterpart record on sphere: {error}");
                }
                // TODO(#257)
                let publishable = if temporary_validate_expiry {
                    record.has_publishable_timeframe()
                } else {
                    true
                };
                if publishable {
                    client.publish(record).await?;
                } else {
                    return Err(anyhow!("Record is expired and cannot be published."));
                }
            }
            NameSystemJob::ResolveAll { context } => {
                let name_stream = {
                    let sphere = context.to_sphere().await?;
                    let names = sphere.get_address_book().await?.get_identities().await?;

                    names.into_stream().await?
                };

                resolve_all(client.clone(), context, name_stream, ipfs_api).await?;
            }
            NameSystemJob::ResolveSince { context, since } => {
                let history_stream = {
                    let sphere = context.to_sphere().await?;
                    sphere.into_history_stream(since.as_ref())
                };

                tokio::pin!(history_stream);

                let reverse_history = history_stream
                    .fold(VecDeque::new(), |mut all, step| {
                        if let Ok(entry) = step {
                            all.push_front(entry);
                        }
                        all
                    })
                    .await;

                let mut names_to_resolve = BTreeMap::<String, IdentityIpld>::new();
                let mut names_to_ignore = BTreeSet::new();

                for (_, sphere) in reverse_history {
                    let names = sphere.get_address_book().await?.get_identities().await?;
                    let changelog = names.load_changelog().await?;

                    for operation in changelog.changes.iter() {
                        match operation {
                            MapOperation::Add { key, value } => {
                                // Walking backwards through history, we will
                                // ignore any name changes where the name has
                                // either been updated or removed in the future
                                if !names_to_ignore.contains(key)
                                    && !names_to_resolve.contains_key(key)
                                {
                                    names_to_resolve.insert(key.clone(), value.clone());
                                }
                            }
                            MapOperation::Remove { key } => {
                                names_to_ignore.insert(key.clone());
                            }
                        };
                    }
                }

                resolve_all(
                    client.clone(),
                    context,
                    tokio_stream::iter(names_to_resolve.into_iter().map(Ok)),
                    ipfs_api,
                )
                .await?;
            }
            NameSystemJob::ResolveImmediately { context, name, tx } => {
                // TODO(#256): This is going to be blocked by any pending
                // "resolve all" jobs. We should consider delaying "resolve
                // all" so that an eager client can get ahead of the queue
                // if desired. Even better would be some kind of streamed
                // priority queue for resolutions, but that's a more
                // involved enhancement.
                let stream = {
                    let sphere = context.to_sphere().await?;
                    let names = sphere.get_address_book().await?.get_identities().await?;
                    let address = names.get(&name).await?;

                    match address {
                        Some(address) => tokio_stream::once(Ok((name.clone(), address.clone()))),
                        None => {
                            let _ = tx.send(None);
                            return Ok(()) as Result<()>;
                        }
                    }
                };

                resolve_all(client.clone(), context.clone(), stream, ipfs_api).await?;

                let cid = context.resolve_petname(&name).await?;

                let _ = tx.send(cid);
            }
        };
        Ok(())
    });

    run_job.await
}

/// Consumes a stream of name / address tuples, resolving them one at a time
/// and updating the provided [SphereContext] with the latest resolved values
async fn resolve_all<C, K, S, N>(
    client: Arc<dyn NameResolver>,
    mut context: C,
    stream: N,
    ipfs_api: &Url,
) -> Result<()>
where
    C: HasMutableSphereContext<K, S>,
    K: KeyMaterial + Clone + 'static,
    S: Storage + 'static,
    N: Stream<Item = Result<(String, IdentityIpld)>>,
{
    tokio::pin!(stream);

    let kubo_client = KuboClient::new(ipfs_api)?;
    let db = context.sphere_context().await?.db().clone();

    let ipfs_store = {
        let inner = db.clone();
        let inner = IpfsStore::new(inner, Some(kubo_client));
        let inner = BlockStoreRetry::new(inner, 6u32, Duration::new(10, 0));
        UcanStore(inner)
    };

    while let Some((name, identity)) = stream.try_next().await? {
        let last_known_record = identity.link_record(&db).await;

        let next_record =
            match fetch_record(client.clone(), name.clone(), identity.did.clone()).await? {
                Some(record) => {
                    // TODO(#257)
                    if false {
                        match record.validate(&ipfs_store).await {
                            Ok(_) => {}
                            Err(error) => {
                                error!("Failed record validation: {}", error);
                                continue;
                            }
                        }
                    }

                    // TODO(#258): Verify that the new value is the most recent value
                    Some(record)
                }
                None => {
                    // TODO(#259): Expire recorded value if we don't get an updated
                    // record after some designated TTL
                    continue;
                }
            };

        match &next_record {
            // TODO(#260): What if the resolved value is None?
            Some(record) if last_known_record != next_record => {
                debug!(
                    "Gateway adopting petname record for '{}' ({}): {}",
                    name, identity.did, record
                );
                context.adopt_petname(&name, record).await?;
            }
            _ => continue,
        }
    }

    if context.has_unsaved_changes().await? {
        SphereCursor::latest(context).save(None).await?;
    }

    Ok(())
}

/// Attempts to fetch a single name record from the name system.
async fn fetch_record(
    client: Arc<dyn NameResolver>,
    name: String,
    identity: Did,
) -> Result<Option<LinkRecord>> {
    debug!("Resolving record '{}' ({})...", name, identity);
    Ok(match client.resolve(&identity).await {
        Ok(Some(record)) => {
            debug!(
                "Resolved record for '{}' ({}): {}",
                name,
                identity,
                record.to_string()
            );
            Some(record)
        }
        Ok(None) => {
            warn!("No record found for {} ({})", name, identity);
            None
        }
        Err(error) => {
            warn!("Failed to resolve '{}' ({}): {:?}", name, identity, error);
            None
        }
    })
}

#[allow(dead_code)]
pub struct OnDemandNameResolver<H>(UnboundedSender<NameSystemJob<H>>);

impl<H> OnDemandNameResolver<H> {
    #[allow(dead_code)]
    pub async fn resolve(&self, context: H, name: &str) -> Result<Option<Cid>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.0
            .send(NameSystemJob::ResolveImmediately {
                context,
                name: name.to_string(),
                tx,
            })
            .map_err(|error| anyhow!(error.to_string()))?;
        Ok(rx.await?)
    }
}

async fn set_counterpart_record<C, K, S>(context: C, record: &LinkRecord) -> Result<()>
where
    C: HasMutableSphereContext<K, S>,
    K: KeyMaterial + Clone + 'static,
    S: Storage + 'static,
{
    debug!("Setting counterpart record...");
    let counterpart_identity = {
        let sphere_context = context.sphere_context().await?;
        let db = sphere_context.db();
        db.require_key::<_, Did>(COUNTERPART).await?
    };
    let counterpart_link_record_key = format!("link_record/{counterpart_identity}");
    let mut cursor = SphereCursor::latest(context.clone());
    cursor
        .write(
            &counterpart_link_record_key,
            &ContentType::Text.to_string(),
            record.encode()?.as_bytes(),
            None,
        )
        .await?;

    cursor.save(None).await?;
    Ok(())
}

async fn get_counterpart_record<C, K, S>(context: &C) -> Result<Option<LinkRecord>>
where
    C: HasMutableSphereContext<K, S>,
    K: KeyMaterial + Clone + 'static,
    S: Storage + 'static,
{
    debug!("Getting counterpart record...");
    let counterpart_identity = {
        let sphere_context = context.sphere_context().await?;
        let db = sphere_context.db();
        db.require_key::<_, Did>(COUNTERPART).await?
    };
    let counterpart_link_record_key = format!("link_record/{counterpart_identity}");

    let mut buffer = String::new();
    if let Some(mut file) = context.read(&counterpart_link_record_key).await? {
        file.contents.read_to_string(&mut buffer).await?;
        Ok(Some(LinkRecord::try_from(buffer)?))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use noosphere_core::authority::{generate_capability, SphereAction};
    use noosphere_ns::helpers::KeyValueNameResolver;
    use noosphere_sphere::helpers::{simulated_sphere_context, SimulationAccess};
    use serde_json::json;
    use ucan::builder::UcanBuilder;

    use super::*;

    #[tokio::test]
    async fn it_publishes_to_the_name_system() -> Result<()> {
        let ipfs_url: Url = "http://127.0.0.1:5000".parse()?;
        let sphere = simulated_sphere_context(SimulationAccess::ReadWrite, None).await?;
        let record: LinkRecord = {
            let context = sphere.lock().await;
            let identity: &str = context.identity().into();
            UcanBuilder::default()
                .issued_by(&context.author().key)
                .for_audience(identity)
                .claiming_capability(&generate_capability(identity, SphereAction::Publish))
                .with_lifetime(1000)
                .with_fact(
                    json!({ "link": "bafyr4iagi6t6khdrtbhmyjpjgvdlwv6pzylxhuhstxhkdp52rju7er325i" }),
                )
                .build()
                .unwrap()
                .sign()
                .await
                .unwrap()
                .into()
        };

        let expired: LinkRecord = {
            let context = sphere.lock().await;
            let identity: &str = context.identity().into();
            UcanBuilder::default()
                .issued_by(&context.author().key)
                .for_audience(identity)
                .claiming_capability(&generate_capability(identity, SphereAction::Publish))
                .with_expiration(ucan::time::now() - 1000)
                .with_fact(
                    json!({ "link": "bafyr4iagi6t6khdrtbhmyjpjgvdlwv6pzylxhuhstxhkdp52rju7er325i" }),
                )
                .build()
                .unwrap()
                .sign()
                .await
                .unwrap()
                .into()
        };

        let mut with_client = TryOrReset::new(|| async { Ok(KeyValueNameResolver::default()) });

        // Valid, unexpired records should be publishable by a gateway
        assert!(process_job(
            NameSystemJob::Publish {
                context: sphere.clone(),
                record,
                temporary_validate_expiry: true,
            },
            &mut with_client,
            &ipfs_url,
        )
        .await
        .is_ok());

        // Expired records should not be publishable by a gateway
        assert!(process_job(
            NameSystemJob::Publish {
                context: sphere.clone(),
                record: expired,
                temporary_validate_expiry: true,
            },
            &mut with_client,
            &ipfs_url,
        )
        .await
        .is_err());

        Ok(())
    }
}
