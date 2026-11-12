// Copyright 2023 The AccessKit Authors. All rights reserved.
// Licensed under the Apache License, Version 2.0 (found in
// the LICENSE-APACHE file) or the MIT license (found in
// the LICENSE-MIT file), at your option.

use accesskit::{ActivationHandler, DeactivationHandler};
use accesskit_atspi_common::{Adapter as AdapterImpl, AppContext, Event};
#[cfg(not(feature = "tokio"))]
use async_channel::{Receiver, Sender};
use atspi::proxy::bus::StatusProxy;
#[cfg(not(feature = "tokio"))]
use futures_util::{pin_mut as pin, select, StreamExt};
use std::{
    sync::{Arc, Mutex, OnceLock, RwLock},
    thread,
};
#[cfg(feature = "tokio")]
use tokio::{
    pin, select,
    sync::mpsc::{UnboundedReceiver as Receiver, UnboundedSender as Sender},
};
#[cfg(feature = "tokio")]
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};
use zbus::{Connection, ConnectionBuilder};

use crate::{
    adapter::{AdapterState, Callback, Message},
    atspi::{map_or_ignoring_broken_pipe, Bus},
    executor::Executor,
    util::block_on,
};

static APP_CONTEXT: OnceLock<Arc<RwLock<AppContext>>> = OnceLock::new();
static MESSAGES: OnceLock<Sender<Message>> = OnceLock::new();

pub(crate) fn get_or_init_app_context<'a>() -> &'a Arc<RwLock<AppContext>> {
    APP_CONTEXT.get_or_init(AppContext::new)
}

pub(crate) fn get_or_init_messages() -> Sender<Message> {
    MESSAGES
        .get_or_init(|| {
            #[cfg(not(feature = "tokio"))]
            let (tx, rx) = async_channel::unbounded();
            #[cfg(feature = "tokio")]
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

            thread::spawn(|| {
                let executor = Executor::new();
                block_on(executor.run(async {
                    if let Ok(session_bus) = ConnectionBuilder::session() {
                        if let Ok(session_bus) = session_bus.internal_executor(false).build().await
                        {
                            // MESH-KVM PATCH: was `.unwrap()`, which aborted the program.
                            if let Err(error) = run_event_loop(&executor, session_bus, rx).await {
                                accessibility_is_off(&error);
                            }
                        }
                    }
                }))
            });

            tx
        })
        .clone()
}

struct AdapterEntry {
    id: usize,
    activation_handler: Box<dyn ActivationHandler>,
    deactivation_handler: Box<dyn DeactivationHandler>,
    state: Arc<Mutex<AdapterState>>,
}

fn activate_adapter(entry: &mut AdapterEntry) {
    let mut state = entry.state.lock().unwrap();
    if let AdapterState::Inactive {
        is_window_focused,
        root_window_bounds,
        action_handler,
    } = &*state
    {
        *state = match entry.activation_handler.request_initial_tree() {
            Some(initial_state) => {
                let r#impl = AdapterImpl::with_wrapped_action_handler(
                    entry.id,
                    get_or_init_app_context(),
                    Callback::new(),
                    initial_state,
                    *is_window_focused,
                    *root_window_bounds,
                    Arc::clone(action_handler),
                );
                AdapterState::Active(r#impl)
            }
            None => AdapterState::Pending {
                is_window_focused: *is_window_focused,
                root_window_bounds: *root_window_bounds,
                action_handler: Arc::clone(action_handler),
            },
        };
    }
}

fn deactivate_adapter(entry: &mut AdapterEntry) {
    let mut state = entry.state.lock().unwrap();
    match &*state {
        AdapterState::Inactive { .. } => (),
        AdapterState::Pending {
            is_window_focused,
            root_window_bounds,
            action_handler,
        } => {
            *state = AdapterState::Inactive {
                is_window_focused: *is_window_focused,
                root_window_bounds: *root_window_bounds,
                action_handler: Arc::clone(action_handler),
            };
            drop(state);
            entry.deactivation_handler.deactivate_accessibility();
        }
        AdapterState::Active(r#impl) => {
            *state = AdapterState::Inactive {
                is_window_focused: r#impl.is_window_focused(),
                root_window_bounds: r#impl.root_window_bounds(),
                action_handler: r#impl.wrapped_action_handler(),
            };
            drop(state);
            entry.deactivation_handler.deactivate_accessibility();
        }
    }
}

// MESH-KVM PATCH: a bus that can't be reached or talked to leaves accessibility off until the next
// IsEnabled change, with one line on stderr, rather than ending the event loop.
fn accessibility_is_off(error: &zbus::Error) {
    eprintln!("accessibility is off: {error}");
}

// MESH-KVM PATCH
async fn bus_if_enabled(
    enabled: zbus::Result<bool>,
    session_bus: &Connection,
    executor: &Executor<'_>,
) -> Option<Bus> {
    let bus = match enabled {
        Ok(true) => map_or_ignoring_broken_pipe(Bus::new(session_bus, executor).await, None, Some),
        Ok(false) => Ok(None),
        Err(error) => Err(error),
    };
    bus.unwrap_or_else(|error| {
        accessibility_is_off(&error);
        None
    })
}

async fn run_event_loop(
    executor: &Executor<'_>,
    session_bus: Connection,
    rx: Receiver<Message>,
) -> zbus::Result<()> {
    let session_bus_copy = session_bus.clone();
    let _session_bus_task = executor.spawn(
        async move {
            loop {
                session_bus_copy.executor().tick().await;
            }
        },
        "accesskit_session_bus_task",
    );

    let status = StatusProxy::new(&session_bus).await?;
    let changes = status.receive_is_enabled_changed().await.fuse();
    pin!(changes);

    #[cfg(not(feature = "tokio"))]
    let messages = rx.fuse();
    #[cfg(feature = "tokio")]
    let messages = UnboundedReceiverStream::new(rx).fuse();
    pin!(messages);

    let mut atspi_bus = None;
    let mut adapters: Vec<AdapterEntry> = Vec::new();

    loop {
        select! {
            change = changes.next() => {
                atspi_bus = None;
                if let Some(change) = change {
                    // MESH-KVM PATCH: was `?` on both, which ended the loop and aborted the program.
                    atspi_bus = bus_if_enabled(change.get().await, &session_bus, executor).await;
                }
                for entry in &mut adapters {
                    if atspi_bus.is_some() {
                        activate_adapter(entry);
                    } else {
                        deactivate_adapter(entry);
                    }
                }
            }
            message = messages.next() => {
                if let Some(message) = message {
                    // MESH-KVM PATCH: was `?`. A bus that fails is let go, as when IsEnabled goes false.
                    if let Err(error) = process_adapter_message(&atspi_bus, &mut adapters, message).await {
                        accessibility_is_off(&error);
                        atspi_bus = None;
                        for entry in &mut adapters {
                            deactivate_adapter(entry);
                        }
                    }
                }
            }
        }
    }
}

async fn process_adapter_message(
    atspi_bus: &Option<Bus>,
    adapters: &mut Vec<AdapterEntry>,
    message: Message,
) -> zbus::Result<()> {
    match message {
        Message::AddAdapter {
            id,
            activation_handler,
            deactivation_handler,
            state,
        } => {
            adapters.push(AdapterEntry {
                id,
                activation_handler,
                deactivation_handler,
                state,
            });
            if atspi_bus.is_some() {
                let entry = adapters.last_mut().unwrap();
                activate_adapter(entry);
            }
        }
        Message::RemoveAdapter { id } => {
            if let Ok(index) = adapters.binary_search_by(|entry| entry.id.cmp(&id)) {
                adapters.remove(index);
            }
        }
        Message::RegisterInterfaces { node, interfaces } => {
            if let Some(bus) = atspi_bus {
                bus.register_interfaces(node, interfaces).await?
            }
        }
        Message::UnregisterInterfaces {
            adapter_id,
            node_id,
            interfaces,
        } => {
            if let Some(bus) = atspi_bus {
                bus.unregister_interfaces(adapter_id, node_id, interfaces)
                    .await?
            }
        }
        Message::EmitEvent {
            adapter_id,
            event: Event::Object { target, event },
        } => {
            if let Some(bus) = atspi_bus {
                bus.emit_object_event(adapter_id, target, event).await?
            }
        }
        Message::EmitEvent {
            adapter_id,
            event:
                Event::Window {
                    target,
                    name,
                    event,
                },
        } => {
            if let Some(bus) = atspi_bus {
                bus.emit_window_event(adapter_id, target, name, event)
                    .await?;
            }
        }
    }

    Ok(())
}

// MESH-KVM PATCH: the failures that used to abort, without a D-Bus daemon. The "session bus" is a
// private peer-to-peer pair, and the accessibility bus is AT_SPI_BUS_ADDRESS, in a directory of
// the test's own. Only with the async-io executor, which is the one mesh-kvm builds.
#[cfg(all(test, not(feature = "tokio")))]
mod mesh_kvm_patch_tests {
    use super::*;
    use std::{
        env, fs,
        os::unix::net::{UnixListener, UnixStream},
        path::PathBuf,
    };
    use zbus::Guid;

    fn private_dir() -> PathBuf {
        let dir = env::temp_dir().join(format!("accesskit-unix-patch-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn peer_to_peer_pair() -> (Connection, Connection) {
        let (one, other) = UnixStream::pair().unwrap();
        let server = ConnectionBuilder::unix_stream(one)
            .server(Guid::generate())
            .unwrap()
            .p2p()
            .build();
        let client = ConnectionBuilder::unix_stream(other).p2p().build();
        let (server, client) = futures_lite::future::zip(server, client).await;
        (server.unwrap(), client.unwrap())
    }

    #[test]
    fn a_bus_that_cant_be_reached_leaves_accessibility_off() {
        let dir = private_dir();
        // Nothing here may reach the desktop's own buses, even by mistake.
        env::set_var(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", dir.join("no-session-bus").display()),
        );

        // A server with one GUID, at an address that says another: the bug as it was seen.
        let socket = dir.join("wrong-guid");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let _ = block_on(
                    ConnectionBuilder::unix_stream(stream)
                        .server(Guid::generate())
                        .unwrap()
                        .p2p()
                        .build(),
                );
            }
        });

        let executor = Executor::new();
        block_on(executor.run(async {
            let (_peer, session_bus) = peer_to_peer_pair().await;

            env::set_var(
                "AT_SPI_BUS_ADDRESS",
                format!("unix:path={},guid={}", socket.display(), Guid::generate()),
            );
            let error = Bus::new(&session_bus, &executor).await.err().unwrap();
            assert!(error.to_string().contains("GUID mismatch"), "{error}");
            assert!(bus_if_enabled(Ok(true), &session_bus, &executor)
                .await
                .is_none());

            env::set_var(
                "AT_SPI_BUS_ADDRESS",
                format!("unix:path={}", dir.join("nobody-listens").display()),
            );
            assert!(bus_if_enabled(Ok(true), &session_bus, &executor)
                .await
                .is_none());

            let failed_get = Err(zbus::Error::Failure("IsEnabled couldn't be read".into()));
            assert!(bus_if_enabled(failed_get, &session_bus, &executor)
                .await
                .is_none());

            assert!(bus_if_enabled(Ok(false), &session_bus, &executor)
                .await
                .is_none());
        }));
        server.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
