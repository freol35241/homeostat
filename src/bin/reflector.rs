//! The reflector adapter: mirrors every command back as the corresponding
//! state (`home/cmd/...` -> `home/state/...`, the envelope's value). Used by
//! the evening-lights fixture houses as the world the automation acts on:
//! a light that instantly obeys. Envelope-less commands are dropped, like
//! any real adapter drops invalid commands.

use std::env;

use tokio::signal::unix::{signal, SignalKind};

use homeostat::bus;

#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() {
    let unit = env::var(bus::ENV_UNIT).unwrap_or_else(|_| "reflector".to_string());
    let endpoint = env::var(bus::ENV_BUS).expect("HOMEOSTAT_BUS must be set");

    let session = zenoh::open(bus::connect_config(&endpoint))
        .await
        .expect("bus session");
    let cmd_sub = session
        .declare_subscriber("home/cmd/**")
        .await
        .expect("cmd subscriber");
    let _token = session
        .liveliness()
        .declare_token(bus::liveliness_key(&unit))
        .await
        .expect("liveliness token");

    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    loop {
        tokio::select! {
            sample = cmd_sub.recv_async() => {
                let Ok(sample) = sample else { break };
                let key = sample
                    .key_expr()
                    .as_str()
                    .replacen("home/cmd/", "home/state/", 1);
                let Ok(envelope) =
                    serde_json::from_slice::<serde_json::Value>(&sample.payload().to_bytes())
                else {
                    continue;
                };
                let Some(value) = envelope.get("value") else { continue };
                let _ = session.put(key, value.to_string()).await;
            }
            _ = term.recv() => break,
        }
    }
    drop(_token);
    let _ = session.close().await;
}
