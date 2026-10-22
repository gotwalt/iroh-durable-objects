//! Dial-back logging when the peer is gone.

use std::time::Duration;

use iroh::{Endpoint, EndpointAddr, RelayMap, RelayMode, endpoint::presets};
use iroh_do_chat_app::{CTL_ALPN, ChatApp, ChatStats, store};
use iroh_do_harness::{Harness, Timings};
use tokio::time::sleep;

/// A dial-back to a peer whose socket has closed is logged as failed.
#[tokio::test(flavor = "multi_thread")]
async fn failed_dial_back_is_logged() {
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<ChatApp>(Timings::default());
    let ep = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Custom(RelayMap::empty()))
        .bind()
        .await
        .unwrap();
    let addr = EndpointAddr::new(obj.id()).with_relay_url(h.relay_url(&obj.id()).parse().unwrap());
    let conn = ep.connect(addr, CTL_ALPN).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(b"/callback 2\n").await.unwrap();
    let mut buf = [0u8; 64];
    let n = recv.read(&mut buf).await.unwrap().unwrap();
    assert!(buf[..n].starts_with(b"ok: callback in 2s"));
    // The peer goes away entirely: its relay socket to the object closes before the alarm.
    conn.close(0u32.into(), b"bye");
    ep.close().await;
    sleep(Duration::from_secs(4)).await;

    let rows = obj.with_sql(|s| store::log_recent(s, 1000).unwrap());
    let cb: Vec<_> = rows.iter().filter(|r| r.kind == "callback").collect();
    assert_eq!(cb.len(), 1, "{rows:?}");
    assert_eq!(cb[0].text, "failed: no socket for peer");
    assert_eq!(chat_stats(&obj).dial_backs_failed, 1);
}

/// The chat app's counters.
#[allow(dead_code)]
fn chat_stats(obj: &iroh_do_harness::DoObject) -> ChatStats {
    obj.with_app(|a: &ChatApp| a.stats())
        .expect("live chat app")
}
