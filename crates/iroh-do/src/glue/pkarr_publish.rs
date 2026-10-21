//! pkarr publish over `fetch` (spec §5.9, design note 004). Two short outbound
//! requests; nothing stays open, so hibernation is unaffected.

use iroh_base::SecretKey;
use worker::{Fetch, Headers, Method, Request, RequestInit};

use crate::pkarr::{build_publish, confirm};

/// PUT the record, then confirm with a GET (a stale PUT also answers 204).
/// `Err` carries a short status string for logs and storage.
pub async fn publish(secret: &SecretKey, relay_url: &str, pkarr_relay: &str) -> Result<(), String> {
    let req = build_publish(secret, relay_url, pkarr_relay).map_err(|e| format!("build: {e:?}"))?;
    let body = js_sys::Uint8Array::from(req.body.as_slice());
    let mut init = RequestInit::new();
    init.with_method(Method::Put).with_body(Some(body.into()));
    let put = Request::new_with_init(&req.url, &init).map_err(|e| format!("put request: {e}"))?;
    let resp = Fetch::Request(put)
        .send()
        .await
        .map_err(|e| format!("put: {e}"))?;
    let code = resp.status_code();
    if !(200..300).contains(&code) {
        return Err(format!("put status {code}"));
    }
    let headers = Headers::new();
    let _ = headers.set("accept", "application/x-pkarr-signed-packet");
    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    let get = Request::new_with_init(&req.url, &init).map_err(|e| format!("get request: {e}"))?;
    let mut resp = Fetch::Request(get)
        .send()
        .await
        .map_err(|e| format!("get: {e}"))?;
    if resp.status_code() != 200 {
        return Err(format!("get status {}", resp.status_code()));
    }
    let got = resp.bytes().await.map_err(|e| format!("get body: {e}"))?;
    if confirm(secret, &got, req.timestamp_us, relay_url) {
        Ok(())
    } else {
        Err("get returned a different record".into())
    }
}
