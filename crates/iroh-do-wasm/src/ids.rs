//! `EndpointId` and `EndpointAddr`, as `@number0/iroh` 1.1.0 names them, plus z-base-32.

use std::str::FromStr;

use wasm_bindgen::prelude::*;

/// An endpoint's identifier, a 32-byte ed25519 public key.
#[wasm_bindgen]
#[derive(Clone, Copy)]
pub struct EndpointId(pub(crate) iroh::EndpointId);

#[wasm_bindgen]
impl EndpointId {
    /// Parses the default text form (`toString`), as iroh's `FromStr` does.
    ///
    /// Do not use for URLs or DNS labels: those carry z-base-32 (`fromZ32`), which `FromStr`
    /// misreads for some inputs (design note 004).
    #[wasm_bindgen(js_name = fromString)]
    pub fn from_string(s: &str) -> Result<EndpointId, JsError> {
        iroh::EndpointId::from_str(s)
            .map(EndpointId)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Parses z-base-32 (52 characters), the form in relay URLs and DNS labels.
    #[wasm_bindgen(js_name = fromZ32)]
    pub fn from_z32(s: &str) -> Result<EndpointId, JsError> {
        iroh::EndpointId::from_z32(s)
            .map(EndpointId)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Constructs an id from its 32 bytes; fails if they are not a valid key.
    #[wasm_bindgen(js_name = fromBytes)]
    pub fn from_bytes(bytes: &[u8]) -> Result<EndpointId, JsError> {
        let arr: &[u8; 32] = bytes
            .try_into()
            .map_err(|_| JsError::new("EndpointId must be 32 bytes"))?;
        iroh::EndpointId::from_bytes(arr)
            .map(EndpointId)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Returns true if both ids are equal.
    pub fn equals(&self, other: &EndpointId) -> bool {
        self.0 == other.0
    }

    /// Returns the 32 bytes.
    #[wasm_bindgen(js_name = toBytes)]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.0.as_bytes().to_vec()
    }

    /// Short form for logs.
    #[wasm_bindgen(js_name = fmtShort)]
    pub fn fmt_short(&self) -> String {
        self.0.fmt_short().to_string()
    }

    /// The default text form.
    #[wasm_bindgen(js_name = toString)]
    pub fn to_string_js(&self) -> String {
        self.0.to_string()
    }

    /// z-base-32, for URLs and DNS labels.
    #[wasm_bindgen(js_name = toZ32)]
    pub fn to_z32(&self) -> String {
        self.0.to_z32()
    }
}

/// An endpoint's id with the addresses it can be reached at. For an iroh-do endpoint that is
/// its relay URL, which it serves itself.
#[wasm_bindgen]
#[derive(Clone)]
pub struct EndpointAddr {
    pub(crate) id: iroh::EndpointId,
    pub(crate) relay_url: Option<String>,
}

#[wasm_bindgen]
impl EndpointAddr {
    /// Creates an address. `addresses` is accepted for `@number0/iroh` compatibility and
    /// ignored: an object has no IP transport.
    #[wasm_bindgen(constructor)]
    pub fn new(
        id: &EndpointId,
        relay_url: Option<String>,
        _addresses: Option<Vec<String>>,
    ) -> EndpointAddr {
        EndpointAddr {
            id: id.0,
            relay_url,
        }
    }

    /// The endpoint id.
    pub fn id(&self) -> EndpointId {
        EndpointId(self.id)
    }

    /// Always empty here.
    #[wasm_bindgen(js_name = directAddresses)]
    pub fn direct_addresses(&self) -> Vec<String> {
        Vec::new()
    }

    /// The relay URL, if known.
    #[wasm_bindgen(js_name = relayUrl)]
    pub fn relay_url(&self) -> Option<String> {
        self.relay_url.clone()
    }

    /// Returns true if both addresses have the same values.
    pub fn equal(&self, other: &EndpointAddr) -> bool {
        self.id == other.id && self.relay_url == other.relay_url
    }

    #[wasm_bindgen(js_name = toString)]
    pub fn to_string_js(&self) -> String {
        match &self.relay_url {
            Some(u) => format!("{} via {u}", self.id.to_z32()),
            None => self.id.to_z32(),
        }
    }
}

/// Reads the peer id out of an `EndpointId` or `EndpointAddr` without consuming the JS object.
///
/// wasm-bindgen's `TryFromJsValue` for exported structs takes ownership of the Rust value and
/// detaches the JS object, so it cannot be used for a by-reference union argument. Both
/// classes are read through their public methods instead.
pub(crate) fn peer_from_js(v: &JsValue) -> Result<iroh::EndpointId, JsValue> {
    let call = |obj: &JsValue, name: &str| -> Result<JsValue, JsValue> {
        let f: js_sys::Function = js_sys::Reflect::get(obj, &JsValue::from_str(name))?
            .dyn_into()
            .map_err(|_| crate::js_err(format!("expected EndpointId or EndpointAddr ({name})")))?;
        f.call0(obj)
    };
    let id = if js_sys::Reflect::has(v, &JsValue::from_str("relayUrl"))? {
        call(v, "id")?
    } else {
        v.clone()
    };
    let bytes = js_sys::Uint8Array::new(&call(&id, "toBytes")?).to_vec();
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| crate::js_err("EndpointId must be 32 bytes"))?;
    iroh::EndpointId::from_bytes(&arr).map_err(crate::js_err)
}
