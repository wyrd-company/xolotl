//! Startup configuration errors and warnings from the background connector.
//! A separate test process gives the log assertions their own subscriber.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use xolotl::config::NatsConfig;
use xolotl::export::nats;

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn local_errors_are_fatal_but_unreachable_nats_warns_and_retries() {
    let logs = Logs::default();
    let writer = logs.clone();
    tracing_subscriber::fmt().with_writer(move || writer.clone()).with_ansi(false).init();
    let cfg = NatsConfig { url: "nats://127.0.0.1:9".into(), ..Default::default() };
    let invalid = NatsConfig { url: "http://[invalid".into(), ..cfg.clone() };
    let err = nats::start(invalid, None).await.err().expect("invalid URL is fatal");
    assert!(format!("{err:#}").contains("export.nats.url"), "{err:#}");
    let missing = tempfile::tempdir().unwrap();
    let invalid = NatsConfig { ca_cert: Some(missing.path().join("missing-ca.pem")), ..cfg.clone() };
    let err = nats::start(invalid, None).await.err().expect("missing CA is fatal");
    assert!(format!("{err:#}").contains("reading NATS CA"), "{err:#}");
    let creds = json!({"creds": "this is not a NATS credentials file"}).as_object().unwrap().clone();
    let err = nats::start(cfg.clone(), Some(creds)).await.err().expect("unparseable creds are fatal");
    assert!(format!("{err:#}").contains("parsing the creds"), "{err:#}");

    let refused = tokio::net::TcpSocket::new_v4().unwrap();
    refused.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    for url in ["nats://xolotl-nats.invalid:4222".to_string(), format!("nats://{}", refused.local_addr().unwrap())] {
        logs.0.lock().unwrap().clear();
        let sink = tokio::time::timeout(Duration::from_secs(1), nats::start(NatsConfig { url, ..cfg.clone() }, None))
            .await
            .expect("no network work on the startup path")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
                if text.matches("NATS export unreachable; rows wait in the queue; retrying").count() >= 2 {
                    assert!(text.contains("WARN"), "{text}");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("failed connections warn and keep retrying");
        let export = xolotl::export::Exporter::new(vec![sink]);
        tokio::time::timeout(Duration::from_secs(1), export.shutdown()).await.expect("empty offline sink stops promptly");
    }
    // ClickHouse builds its HTTP client without attempting a connection.
    let ch = xolotl::export::clickhouse::start(
        xolotl::config::ClickHouseConfig { url: "http://xolotl-clickhouse.invalid:8123".into(), ..Default::default() },
        None,
        None,
    )
    .expect("unreachable ClickHouse does not prevent startup");
    xolotl::export::Exporter::new(vec![ch]).shutdown().await;
}
