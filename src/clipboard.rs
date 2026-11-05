use arboard::{Clipboard, ImageData};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize)]
pub enum ClipboardPayload {
    Text(String),
    Image {
        width: usize,
        height: usize,
        bytes: Vec<u8>,
    },
}

fn hash_bytes(data: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

pub fn start_clipboard_sync(peer_ip: String, port: u16) -> Result<(), String> {
    let last_hash = Arc::new(Mutex::new(0u64));
    let last_hash_rx = Arc::clone(&last_hash);

    // 1. Spawn TCP receiver on Port 24852
    std::thread::Builder::new()
        .name("mesh-kvm-clip-rx".to_string())
        .spawn(move || {
            let bind_addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
            let listener = match TcpListener::bind(bind_addr) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[Clipboard] Failed to bind TCP {}: {}", bind_addr, e);
                    return;
                }
            };
            println!("[Clipboard] Unified clipboard sync listening on TCP {}", bind_addr);

            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut len_buf = [0u8; 4];
                if stream.read_exact(&mut len_buf).is_ok() {
                    let len = u32::from_le_bytes(len_buf) as usize;
                    if len > 0 && len <= 64 * 1024 * 1024 {
                        let mut buf = vec![0u8; len];
                        if stream.read_exact(&mut buf).is_ok() {
                            if let Ok(payload) = bincode::deserialize::<ClipboardPayload>(&buf) {
                                if let Ok(mut cb) = Clipboard::new() {
                                    match payload {
                                        ClipboardPayload::Text(text) => {
                                            let h = hash_bytes(text.as_bytes());
                                            if let Ok(mut guard) = last_hash_rx.lock() {
                                                *guard = h;
                                            }
                                            let _ = cb.set_text(text);
                                        }
                                        ClipboardPayload::Image {
                                            width,
                                            height,
                                            bytes,
                                        } => {
                                            let h = hash_bytes(&bytes);
                                            if let Ok(mut guard) = last_hash_rx.lock() {
                                                *guard = h;
                                            }
                                            let _ = cb.set_image(ImageData {
                                                width,
                                                height,
                                                bytes: Cow::Owned(bytes),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;

    // 2. Spawn clipboard watcher and TCP sender
    std::thread::Builder::new()
        .name("mesh-kvm-clip-tx".to_string())
        .spawn(move || {
            let target_addr: SocketAddr = match format!("{}:{}", peer_ip, port).parse() {
                Ok(a) => a,
                Err(_) => return,
            };

            let mut cb = match Clipboard::new() {
                Ok(c) => c,
                Err(_) => return,
            };

            loop {
                std::thread::sleep(Duration::from_millis(350));

                if let Ok(text) = cb.get_text() {
                    if !text.is_empty() {
                        let h = hash_bytes(text.as_bytes());
                        let changed = if let Ok(mut guard) = last_hash.lock() {
                            if *guard != h {
                                *guard = h;
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        if changed {
                            send_clipboard_payload(target_addr, &ClipboardPayload::Text(text));
                            continue;
                        }
                    }
                }

                if let Ok(img) = cb.get_image() {
                    if !img.bytes.is_empty() {
                        let h = hash_bytes(&img.bytes);
                        let changed = if let Ok(mut guard) = last_hash.lock() {
                            if *guard != h {
                                *guard = h;
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        if changed {
                            send_clipboard_payload(
                                target_addr,
                                &ClipboardPayload::Image {
                                    width: img.width,
                                    height: img.height,
                                    bytes: img.bytes.into_owned(),
                                },
                            );
                        }
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(())
}

fn send_clipboard_payload(target_addr: SocketAddr, payload: &ClipboardPayload) {
    if let Ok(encoded) = bincode::serialize(payload) {
        if let Ok(mut stream) =
            TcpStream::connect_timeout(&target_addr, Duration::from_millis(500))
        {
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            let len_bytes = (encoded.len() as u32).to_le_bytes();
            let _ = stream.write_all(&len_bytes);
            let _ = stream.write_all(&encoded);
        }
    }
}
