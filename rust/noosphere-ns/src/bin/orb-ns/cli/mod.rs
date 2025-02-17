mod address;
mod cli_implementation;
mod processor;

pub use cli_implementation::*;

pub use processor::{process_args, process_command};

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::Result;
    use cid::Cid;
    use noosphere::key::{InsecureKeyStorage, KeyStorage};
    use noosphere_core::authority::{generate_capability, SphereAction};
    use noosphere_core::data::{Did, LinkRecord};
    use noosphere_core::view::SPHERE_LIFETIME;
    use noosphere_ns::{Multiaddr, PeerId};
    use serde::Deserialize;
    use serde_json::json;
    use tokio;
    use tokio::sync::oneshot;
    use ucan::builder::UcanBuilder;
    use ucan::crypto::KeyMaterial;
    use url::Url;

    #[derive(Debug, Deserialize)]
    struct RunnerData {
        #[allow(dead_code)]
        listening_address: Option<Multiaddr>,
        #[allow(dead_code)]
        api_address: Option<Url>,
        #[allow(dead_code)]
        peer_id: PeerId,
    }

    async fn spawn_runner(
        key_name: String,
        key_storage: InsecureKeyStorage,
    ) -> Result<(RunnerData, tokio::task::JoinHandle<Result<()>>)> {
        let (tx, rx) = oneshot::channel::<String>();
        let handle = tokio::spawn(async move {
            let mut response = process_command(
                CLICommand::Run {
                    config: None,
                    key: Some(key_name),
                    listening_address: Some("/ip4/127.0.0.1/tcp/0".parse().unwrap()),
                    api_address: Some("127.0.0.1:0".parse().unwrap()),
                    peers: None,
                    no_default_peers: true,
                    ipfs_api_url: None,
                },
                &key_storage,
            )
            .await
            .unwrap();
            assert!(response.value().is_some());
            tx.send(response.value().unwrap().to_owned()).unwrap();
            response.wait_until_completion().await?;
            Ok(())
        });

        match rx.await {
            Ok(json_str) => {
                let runner_data = serde_json::from_str::<RunnerData>(&json_str).unwrap();
                Ok((runner_data, handle))
            }
            Err(_) => Err(anyhow::anyhow!("sender dropped")),
        }
    }

    #[tokio::test]
    async fn it_processes_record_commands() -> Result<()> {
        let temp_dir = tempfile::Builder::new()
            .prefix("orb-ns-processes-record-commands")
            .tempdir()?;
        let key_storage = InsecureKeyStorage::new(temp_dir.path())?;
        let key_a = key_storage.create_key("key-a").await?;
        let key_b = key_storage.create_key("key-b").await?;
        let _id_a = Did::from(key_a.get_did().await?);
        let id_b = Did::from(key_b.get_did().await?);

        let (runner_a, _handle_a) = spawn_runner("key-a".into(), key_storage.clone()).await?;
        let (runner_b, _handle_b) = spawn_runner("key-b".into(), key_storage.clone()).await?;
        let listener_a = runner_a.listening_address.as_ref().unwrap().to_owned();
        let _listener_b = runner_b.listening_address.as_ref().unwrap().to_owned();
        let api_a = runner_a.api_address.as_ref().unwrap().to_owned();
        let api_b = runner_b.api_address.as_ref().unwrap().to_owned();

        // Request node B to dial node A
        assert!(process_command(
            CLICommand::Peers(CLIPeers::Add {
                api_url: api_b.clone(),
                peer: listener_a.clone(),
            }),
            &key_storage,
        )
        .await
        .unwrap()
        .value()
        .is_none());

        // Wait until nodes are peered.
        loop {
            let res = process_command(
                CLICommand::Peers(CLIPeers::Ls {
                    api_url: api_a.clone(),
                }),
                &key_storage,
            )
            .await
            .unwrap();
            let value = res.value().unwrap();

            if !serde_json::from_str::<Vec<noosphere_ns::Peer>>(value)
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        let link = "bafyr4iagi6t6khdrtbhmyjpjgvdlwv6pzylxhuhstxhkdp52rju7er325i";
        let cid_link: Cid = link.parse()?;
        let ucan = UcanBuilder::default()
            .issued_by(&key_b)
            .for_audience(&id_b)
            .claiming_capability(&generate_capability(&id_b, SphereAction::Publish))
            .with_fact(json!({ "link": cid_link.to_string() }))
            .with_lifetime(SPHERE_LIFETIME)
            .build()?
            .sign()
            .await?;
        let record = LinkRecord::try_from(ucan)?;

        // Push record from node B (for node B)
        assert!(process_command(
            CLICommand::Records(CLIRecords::Put {
                record: record.clone(),
                api_url: api_b.clone(),
            }),
            &key_storage,
        )
        .await
        .unwrap()
        .value()
        .is_none());

        // Pull record for node B from node A
        let res = process_command(
            CLICommand::Records(CLIRecords::Get {
                identity: id_b.clone(),
                api_url: api_a.clone(),
            }),
            &key_storage,
        )
        .await
        .unwrap();
        let value = res.value().unwrap();
        let fetched = serde_json::from_str::<LinkRecord>(value).unwrap();
        assert_eq!(fetched.get_link().unwrap(), cid_link);
        assert_eq!(fetched.sphere_identity(), &id_b);

        Ok(())
    }
}
