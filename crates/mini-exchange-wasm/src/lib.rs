//! WebAssembly entry points for the live demo.
//!
//! The matching core stays in `mini-exchange` and forbids unsafe code. This
//! crate is the `wasm-bindgen` shell: it steps [`DemoSession`] and returns JSON
//! the page can render. Host tests cover the session without a wasm target.

mod session;

use std::str::FromStr;

use session::DemoSession;
use wasm_bindgen::prelude::*;

use mini_exchange::sim::Preset;

/// Browser handle. Construct with a preset name and a decimal seed.
#[wasm_bindgen]
pub struct ExchangeDemo {
    inner: DemoSession,
}

#[wasm_bindgen]
impl ExchangeDemo {
    /// `preset` is `equity` or `prediction-market`. `seed` is a base-10 integer.
    #[wasm_bindgen(constructor)]
    pub fn new(preset: &str, seed: &str) -> Result<ExchangeDemo, JsValue> {
        #[cfg(target_arch = "wasm32")]
        console_error_panic_hook::set_once();
        let preset = Preset::from_str(preset.trim())
            .map_err(|_| JsValue::from_str("preset must be equity or prediction-market"))?;
        let seed = parse_seed(seed).map_err(|err| JsValue::from_str(&err))?;
        Ok(ExchangeDemo {
            inner: DemoSession::new(preset, seed),
        })
    }

    /// Apply `n` simulator events. Returns the snapshot, including new prints.
    pub fn step(&mut self, n: u32) -> String {
        self.inner.advance(n);
        self.inner.snapshot_json(None, None)
    }

    /// Current book and digest, with an empty print batch.
    pub fn snapshot(&mut self) -> String {
        self.inner.advance(0);
        self.inner.snapshot_json(None, None)
    }

    /// Rest a limit. `price` and `qty` are integer ticks and shares.
    pub fn limit(&mut self, market: &str, side: &str, price: u32, qty: u32) -> String {
        match self
            .inner
            .submit_limit(market, side, u64::from(price), u64::from(qty))
        {
            Ok(view) => self.inner.snapshot_json(Some(&view), None),
            Err(err) => self.inner.snapshot_json(None, Some(&err)),
        }
    }

    /// Cancel `id` on `market`. `id` is the decimal order id from the ladder.
    pub fn cancel(&mut self, market: &str, id: &str) -> String {
        let parsed = match id.trim().parse::<u64>() {
            Ok(id) => id,
            Err(_) => {
                return self
                    .inner
                    .snapshot_json(None, Some("order id must be an integer"));
            }
        };
        match self.inner.submit_cancel(market, parsed) {
            Ok(view) => self.inner.snapshot_json(Some(&view), None),
            Err(err) => self.inner.snapshot_json(None, Some(&err)),
        }
    }
}

fn parse_seed(text: &str) -> Result<u64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("seed is empty".to_string());
    }
    text.parse::<u64>()
        .map_err(|_| "seed must be an integer from 0 to 2^64-1".to_string())
}
