//! pkarr publish over `fetch` (spec §5.9, design note 004). Two short outbound
//! requests; nothing stays open, so hibernation is unaffected.

use std::time::Duration;

use iroh_base::SecretKey;
use worker::{AbortController, Fetch, Headers, Method, Request, RequestInit};

use crate::pkarr::{build_publish, confirm};

/// Longest wait for one request, including its response body. Mint waits for the publish,
/// and an outbound request that never answers would keep the object in memory.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// PUT the record, then confirm with a GET (a stale PUT also answers 204).
/// `Err` carries a short status string for logs and storage.
pub async fn publish(secret: &SecretKey, relay_url: &str, pkarr_relay: &str) -> Result<(), String> {
    let req = build_publish(secret, relay_url, pkarr_relay).map_err(|e| format!("build: {e}"))?;
    let body = js_sys::Uint8Array::from(req.body.as_slice());
    let mut init = RequestInit::new();
    init.with_method(Method::Put).with_body(Some(body.into()));
    let put = Request::new_with_init(&req.url, &init).map_err(|e| format!("put request: {e}"))?;
    let (code, _) = send(put, "put").await?;
    if !(200..300).contains(&code) {
        return Err(format!("put status {code}"));
    }
    let headers = Headers::new();
    let _ = headers.set("accept", "application/x-pkarr-signed-packet");
    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    let get = Request::new_with_init(&req.url, &init).map_err(|e| format!("get request: {e}"))?;
    let (code, got) = send(get, "get").await?;
    if code != 200 {
        return Err(format!("get status {code}"));
    }
    if confirm(secret, &got, req.timestamp_us, relay_url) {
        Ok(())
    } else {
        Err("get returned a different record".into())
    }
}

/// Sends `req` and reads its whole body, whatever the status, so that nothing of the
/// response stays pending; aborts the request after [`REQUEST_TIMEOUT`].
async fn send(req: Request, what: &str) -> Result<(u16, Vec<u8>), String> {
    let controller = AbortController::default();
    let signal = controller.signal();
    let exchange = async {
        let mut resp = Fetch::Request(req)
            .send_with_signal(&signal)
            .await
            .map_err(|e| format!("{what}: {e}"))?;
        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("{what} body: {e}"))?;
        Ok((resp.status_code(), body))
    };
    // The timer is cleared when the exchange wins: dropping n0-future's `Sleep` clears its
    // `setTimeout`, and a pending timer would keep the object from hibernating.
    let timeout = async {
        n0_future::time::sleep(REQUEST_TIMEOUT).await;
        Err(format!(
            "{what}: no answer within {}s",
            REQUEST_TIMEOUT.as_secs()
        ))
    };
    let out = n0_future::future::race(exchange, timeout).await;
    if out.is_err() {
        controller.abort();
    }
    out
}
