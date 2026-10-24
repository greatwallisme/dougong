// Rust guideline compliant 2026-10-21
//! Greeter component implementation against the dougong plugin world.
//!
//! Guest state lives in a thread-local because wasm execution is
//! single-threaded; the host serializes every export call, so plain
//! interior mutability is sufficient.

use std::cell::RefCell;

use crate::bindings::dougong::kernel::host;
use crate::bindings::dougong::kernel::types::{
    CallContext, EventKey, KernelError, LogLevel, PluginDescriptor, ServiceKey,
};
use crate::bindings::exports::dougong::kernel::plugin::Guest;

/// Contract hash of the built-in dynamic channel (interface + encoding).
const ABI_HASH: &[u8] = b"dougong-dynamic-channel-v1";

/// Method selector: returns the greeting bytes.
const GREETER_HELLO: u32 = 0;

/// Listener id: replies to `ping` events.
const LISTENER_PING: u64 = 1;

/// Listener id: upper-cases waterfall payloads.
const LISTENER_UPPER: u64 = 2;

struct State {
    invocations: u32,
    lifecycle: u64,
    trap_teardown: bool,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State {
        invocations: 0,
        lifecycle: 0,
        trap_teardown: false,
    });
}

/// Fault-injection hook (S7): a config carrying `"trap_teardown":true`
/// arms the teardown trap. The guest intentionally depends on wit-bindgen
/// only, so the compact host encoding is matched as raw bytes instead of
/// pulling in a JSON parser.
fn asks_trap_teardown(config: &[u8]) -> bool {
    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
    contains(config, b"trap_teardown") && contains(config, b"true")
}

/// The greeter wasm plugin.
#[derive(Debug)]
pub struct Greeter;

fn service_key(name: &str) -> ServiceKey {
    ServiceKey {
        name: name.to_string(),
        abi_hash: ABI_HASH.to_vec(),
    }
}

fn event_key(name: &str) -> EventKey {
    EventKey {
        name: name.to_string(),
        abi_hash: ABI_HASH.to_vec(),
    }
}

fn log(level: LogLevel, msg: &str) {
    host::log(level, msg);
}

fn internal(msg: impl Into<String>) -> KernelError {
    KernelError::Internal(msg.into())
}

impl Guest for Greeter {
    fn descriptor() -> PluginDescriptor {
        PluginDescriptor {
            name: "greeter-wasm".to_string(),
            version: "0.1.0".to_string(),
            inject: vec![],
            provide: vec![service_key("greeting")],
        }
    }

    fn apply(_site: CallContext, config: Vec<u8>) -> Result<(), KernelError> {
        log(LogLevel::Info, "greeter-wasm: applying; providing greeting");
        STATE.with(|state| state.borrow_mut().trap_teardown = asks_trap_teardown(&config));
        host::provide_service(&service_key("greeting")).map_err(|_| {
            internal("host rejected provide-service for declared service \"greeting\"")
        })?;
        host::register_listener(&event_key("ping"), LISTENER_PING)
            .map_err(|_| internal("host rejected listener registration for \"ping\""))?;
        host::register_listener(&event_key("upper"), LISTENER_UPPER)
            .map_err(|_| internal("host rejected listener registration for \"upper\""))?;
        let handle = host::effect_collect("greeter-wasm.invocations")
            .map_err(|_| internal("host rejected effect-collect"))?;
        STATE.with(|state| state.borrow_mut().lifecycle = handle);
        Ok(())
    }

    fn handle_service_call(
        key: ServiceKey,
        method: u32,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, KernelError> {
        if key.name != "greeting" || key.abi_hash != ABI_HASH {
            return Err(KernelError::CapabilityDenied(format!(
                "component only provides \"greeting\", not \"{}\"",
                key.name
            )));
        }
        if method != GREETER_HELLO {
            return Err(KernelError::Internal(format!(
                "unknown method selector {method}"
            )));
        }
        // Fault-injection hook (S7): the "boom" payload traps the guest.
        assert!(
            payload != b"boom",
            "fault-injection hook: service-call trap"
        );
        let count = STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.invocations += 1;
            state.invocations
        });
        Ok(format!("hello from a wasm component (call {count})").into_bytes())
    }

    fn handle_event(
        ev: EventKey,
        listener: u64,
        payload: Vec<u8>,
        next: Option<u64>,
    ) -> Result<Vec<u8>, KernelError> {
        match listener {
            LISTENER_PING => {
                if ev.name != "ping" {
                    return Err(KernelError::CapabilityDenied(format!(
                        "listener 1 answers \"ping\", not \"{}\"",
                        ev.name
                    )));
                }
                // Fault-injection hooks (S7): the JSON string "boom" traps
                // the guest, "fail" returns a plain guest error (event
                // payloads arrive JSON-encoded, hence the quotes).
                assert!(
                    payload != b"\"boom\"",
                    "fault-injection hook: listener trap"
                );
                if payload == b"\"fail\"" {
                    return Err(internal("fault-injection hook: listener failure"));
                }
                log(LogLevel::Info, "greeter-wasm: ping received");
                // Event replies are JSON-encoded kernel values (a bare
                // string must be quoted to stay valid JSON).
                Ok(b"\"pong from greeter-wasm\"".to_vec())
            }
            LISTENER_UPPER => {
                let Some(token) = next else {
                    return Err(internal("upper listener expects a waterfall token"));
                };
                // Pull the rest of the chain, then post-process the result.
                let remainder = host::waterfall_next(token, &payload)
                    .map_err(|_| internal("waterfall-next rejected the continuation token"))?;
                Ok(remainder.to_ascii_uppercase())
            }
            other => Err(internal(format!("unknown listener id {other}"))),
        }
    }

    fn run_disposer(handle: u64) -> Result<(), KernelError> {
        if handle == 0 {
            let (count, trap) = STATE.with(|state| {
                let state = state.borrow();
                (state.invocations, state.trap_teardown)
            });
            // Fault-injection hook (S7): armed by the apply config.
            assert!(!trap, "fault-injection hook: teardown trap");
            log(
                LogLevel::Info,
                &format!("greeter-wasm: teardown after {count} invocation(s)"),
            );
            return Ok(());
        }
        let collected = STATE.with(|state| state.borrow().lifecycle);
        if handle != collected {
            return Err(internal(format!("unknown disposer handle {handle}")));
        }
        log(
            LogLevel::Info,
            "greeter-wasm: collected disposer ran (invocation counters released)",
        );
        Ok(())
    }
}
