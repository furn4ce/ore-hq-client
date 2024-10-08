use std::{ops::{ControlFlow, Range}, sync::Arc, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
use clap::{arg, Parser};
use drillx_2::equix;
use futures_util::{SinkExt, StreamExt};
use solana_sdk::{signature::Keypair, signer::Signer};
use tokio::sync::{mpsc::UnboundedSender, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::{handshake::client::{generate_key, Request}, Message}};
use base64::prelude::*;
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::env;

#[derive(Debug)]
pub enum ServerMessage {
    StartMining([u8; 32], Range<u64>, u64)
}

#[derive(Debug, Parser)]
pub struct MineArgs {
    #[arg(
        long,
        value_name = "threads",
        default_value = "4",
        help = "Number of threads to use while mining"
    )]
    pub threads: u32,
    #[arg(
        long,
        value_name = "BUFFER",
        default_value = "0",
        help = "Buffer time in seconds, to send the submission to the server earlier"
    )]
    pub buffer: u32,
}

pub async fn mine(args: MineArgs, key: Keypair, url: String, unsecure: bool) {
    let running = Arc::new(AtomicBool::new(true));

    loop {
        if !running.load(Ordering::SeqCst) {
            break;
        }

        let base_url = url.clone();
        let mut ws_url_str = if unsecure {
            format!("ws://{}", url)
        } else {
            format!("wss://{}", url)
        };

        if ws_url_str.chars().last().unwrap() != '/' {
            ws_url_str.push('/');
        }

        let client = reqwest::Client::new();

        let http_prefix = if unsecure {
            "http".to_string()
        } else {
            "https".to_string()
        };

        let timestamp = if let Ok(response) = client.get(format!("{}://{}/timestamp", http_prefix, base_url)).send().await {
            if let Ok(ts) = response.text().await {
                if let Ok(ts) = ts.parse::<u64>() {
                    ts
                } else {
                    println!("Server response body for /timestamp failed to parse, contact admin.");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            } else {
                println!("Server response body for /timestamp is empty, contact admin.");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        } else {
            println!("Server restarting, trying again in 3 seconds...");
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        };
        println!("Server Timestamp: {}", timestamp);

        let ts_msg = timestamp.to_le_bytes();
        let sig = key.sign_message(&ts_msg);

        ws_url_str.push_str(&format!("?timestamp={}", timestamp));
        let url = url::Url::parse(&ws_url_str).expect("Failed to parse server url");
        let host = url.host_str().expect("Invalid host in server url");
        let threads = args.threads;

        let auth = BASE64_STANDARD.encode(format!("{}:{}", key.pubkey(), sig));

        println!("Connecting to server...");
        let request = Request::builder()
            .method("GET")
            .uri(url.to_string())
            .header("Sec-Websocket-Key", generate_key())
            .header("Host", host)
            .header("Upgrade", "websocket")
            .header("Connection", "upgrade")
            .header("Sec-Websocket-Version", "13")
            .header("Authorization", format!("Basic {}", auth))
            .body(())
            .unwrap();

        match connect_async(request).await {
            Ok((ws_stream, _)) => {
                println!("Connected to network!");

                let (mut sender, mut receiver) = ws_stream.split();
                let (message_sender, mut message_receiver) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();

                let receiver_thread = tokio::spawn(async move {
                    while let Some(Ok(message)) = receiver.next().await {
                        if process_message(message, message_sender.clone()).is_break() {
                            break;
                        }
                    }
                });

                // send Ready message
                let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("Time went backwards").as_secs();

                let msg = now.to_le_bytes();
                let sig = key.sign_message(&msg).to_string().as_bytes().to_vec();
                let mut bin_data: Vec<u8> = Vec::new();
                bin_data.push(0u8);
                bin_data.extend_from_slice(&key.pubkey().to_bytes());
                bin_data.extend_from_slice(&msg);
                bin_data.extend(sig);

                let _ = sender.send(Message::Binary(bin_data)).await;

                let sender = Arc::new(Mutex::new(sender));

                // receive messages
                let message_sender = sender.clone();
                while let Some(msg) = message_receiver.recv().await {
                    if !running.load(Ordering::SeqCst) {
                        break;
                    }
                
                    match msg {
                        ServerMessage::StartMining(challenge, nonce_range, cutoff) => {
                            // Adjust the cutoff with the buffer
                            let mut cutoff = cutoff.saturating_sub(args.buffer as u64);
                            if cutoff > 60 {
                                cutoff = 55;
                            }

                            // Detect if running on Windows and set symbols accordingly
                            let pb = if env::consts::OS == "windows" {
                                ProgressBar::new_spinner().with_style(
                                    ProgressStyle::default_spinner()
                                        .tick_strings(&["-", "\\", "|", "/"]) // Use simple ASCII symbols
                                        .template("{spinner:.green} {msg}")
                                        .expect("Failed to set progress bar template"),
                                )
                            } else {
                                ProgressBar::new_spinner().with_style(
                                    ProgressStyle::default_spinner()
                                        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
                                        .template("{spinner:.red} {msg}")
                                        .expect("Failed to set progress bar template"),
                                )
                            };

                            println!();
                            pb.set_message("Mining...");
                            pb.enable_steady_tick(Duration::from_millis(120));

                            // Original mining code
                            let hash_timer = Instant::now();
                            let core_ids = core_affinity::get_core_ids().unwrap();
                            let nonces_per_thread = 10_000;
                            let handles = core_ids
                                .into_iter()
                                .map(|i| {
                                    let running = running.clone(); // Capture running in thread
                                    std::thread::spawn({
                                        let mut memory = equix::SolverMemory::new();
                                        move || {
                                            if (i.id as u32).ge(&threads) {
                                                return None;
                                            }

                                            let _ = core_affinity::set_for_current(i);

                                            let first_nonce = nonce_range.start + (nonces_per_thread * (i.id as u64));
                                            let mut nonce = first_nonce;
                                            let mut best_nonce = nonce;
                                            let mut best_difficulty = 0;
                                            let mut best_hash = drillx_2::Hash::default();
                                            let mut total_hashes: u64 = 0;

                                            loop {
                                                // Check if Ctrl+C was pressed
                                                if !running.load(Ordering::SeqCst) {
                                                    return None;
                                                }

                                                // Create hash
                                                for hx in drillx_2::get_hashes_with_memory(&mut memory, &challenge, &nonce.to_le_bytes()) {
                                                    total_hashes += 1;
                                                    let difficulty = hx.difficulty();
                                                    if difficulty.gt(&best_difficulty) {
                                                        best_nonce = nonce;
                                                        best_difficulty = difficulty;
                                                        best_hash = hx;
                                                    }
                                                }

                                                // Exit if processed nonce range
                                                if nonce >= nonce_range.end {
                                                    break;
                                                }

                                                if nonce % 100 == 0 {
                                                    if hash_timer.elapsed().as_secs().ge(&cutoff) {
                                                        if best_difficulty.ge(&8) {
                                                            break;
                                                        }
                                                    }
                                                }

                                                // Increment nonce
                                                nonce += 1;
                                            }

                                            // Return the best nonce
                                            Some((best_nonce, best_difficulty, best_hash, total_hashes))
                                        }
                                    })
                                })
                                .collect::<Vec<_>>();

                            // Join handles and return best nonce
                            let mut best_nonce: u64 = 0;
                            let mut best_difficulty = 0;
                            let mut best_hash = drillx_2::Hash::default();
                            let mut total_nonces_checked = 0;
                            for h in handles {
                                if let Ok(Some((nonce, difficulty, hash, nonces_checked))) = h.join() {
                                    total_nonces_checked += nonces_checked;
                                    if difficulty > best_difficulty {
                                        best_difficulty = difficulty;
                                        best_nonce = nonce;
                                        best_hash = hash;
                                    }
                                }
                            }

                            let hash_time = hash_timer.elapsed();

                            // Stop the spinner after mining is done
                            pb.finish_and_clear();
                            println!("✔ Mining complete!");
                            println!("Processed: {}", total_nonces_checked);
                            println!("Hash time: {:?}", hash_time);
                            let hash_time_secs = hash_time.as_secs();
                            if hash_time_secs > 0 {
                                println!("Hashpower: {:?} H/s", total_nonces_checked.saturating_div(hash_time_secs));
                            }

                            // Send results to the server
                            let message_type = 2u8; // 1 u8 - BestSolution Message
                            let best_hash_bin = best_hash.d; // 16 u8
                            let best_nonce_bin = best_nonce.to_le_bytes(); // 8 u8

                            let mut hash_nonce_message = [0; 24];
                            hash_nonce_message[0..16].copy_from_slice(&best_hash_bin);
                            hash_nonce_message[16..24].copy_from_slice(&best_nonce_bin);
                            let signature = key.sign_message(&hash_nonce_message).to_string().as_bytes().to_vec();

                            let mut bin_data = [0; 57];
                            bin_data[00..1].copy_from_slice(&message_type.to_le_bytes());
                            bin_data[01..17].copy_from_slice(&best_hash_bin);
                            bin_data[17..25].copy_from_slice(&best_nonce_bin);
                            bin_data[25..57].copy_from_slice(&key.pubkey().to_bytes());

                            let mut bin_vec = bin_data.to_vec();
                            bin_vec.extend(signature);

                            {
                                let mut message_sender = message_sender.lock().await;
                                let _ = message_sender.send(Message::Binary(bin_vec)).await;
                            }

                            tokio::time::sleep(Duration::from_secs(5 + args.buffer as u64)).await;

                            let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("Time went backwards").as_secs();

                            let msg = now.to_le_bytes();
                            let sig = key.sign_message(&msg).to_string().as_bytes().to_vec();
                            let mut bin_data: Vec<u8> = Vec::new();
                            bin_data.push(0u8);
                            bin_data.extend_from_slice(&key.pubkey().to_bytes());
                            bin_data.extend_from_slice(&msg);
                            bin_data.extend(sig);
                            {
                                let mut message_sender = message_sender.lock().await;
                                let _ = message_sender.send(Message::Binary(bin_data)).await;
                            }
                        }
                    }
                }                    

                let _ = receiver_thread.await;
            }, 
            Err(e) => {
                match e {
                    tokio_tungstenite::tungstenite::Error::Http(e) => {
                        if let Some(body) = e.body() {
                            println!("Error: {:?}", String::from_utf8(body.to_vec()));
                        } else {
                            println!("Http Error: {:?}", e);
                        }
                    }, 
                    _ => {
                        println!("Error: {:?}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

fn process_message(msg: Message, message_channel: UnboundedSender<ServerMessage>) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(t)=>{
            println!("{}",t);
        },
        Message::Binary(b) => {
            let message_type = b[0];
            match message_type {
                0 => {
                    if b.len() < 49 {
                        println!("Invalid data for Message StartMining");
                    } else {
                        let mut hash_bytes = [0u8; 32];
                        // extract 256 bytes (32 u8's) from data for hash
                        let mut b_index = 1;
                        for i in 0..32 {
                            hash_bytes[i] = b[i + b_index];
                        }
                        b_index += 32;

                        // extract 64 bytes (8 u8's)
                        let mut cutoff_bytes = [0u8; 8];
                        for i in 0..8 {
                            cutoff_bytes[i] = b[i + b_index];
                        }
                        b_index += 8;
                        let cutoff = u64::from_le_bytes(cutoff_bytes);

                        let mut nonce_start_bytes = [0u8; 8];
                        for i in 0..8 {
                            nonce_start_bytes[i] = b[i + b_index];
                        }
                        b_index += 8;
                        let nonce_start = u64::from_le_bytes(nonce_start_bytes);

                        let mut nonce_end_bytes = [0u8; 8];
                        for i in 0..8 {
                            nonce_end_bytes[i] = b[i + b_index];
                        }
                        let nonce_end = u64::from_le_bytes(nonce_end_bytes);

                        let msg = ServerMessage::StartMining(hash_bytes, nonce_start..nonce_end, cutoff);

                        let _ = message_channel.send(msg);
                    }
                },
                _ => {
                    println!("Failed to parse server message type");
                }
            }
        },
        Message::Ping(_) => {}, 
        Message::Pong(_) => {}, 
        Message::Close(v) => {
            println!("Got Close: {:?}", v);
            return ControlFlow::Break(());
        }, 
        _ => {}
    }

    ControlFlow::Continue(())
}
