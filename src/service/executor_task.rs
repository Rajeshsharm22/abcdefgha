//! Service task that processes Wasm executions requests.

use crate::{block, executor, storage};

use alloc::sync::Arc;
use core::{cmp, convert::TryFrom as _, pin::Pin};
use futures::{
    channel::{mpsc, oneshot},
    prelude::*,
};
use hashbrown::HashMap;
use primitive_types::H256;

/// Message that can be sent to the executors task by the other parts of the code.
pub enum ToExecutor {
    /// Call the runtime to apply a block on the state.
    Execute {
        /// Block to try execute.
        to_execute: block::Block,
        /// Channel where to send back the outcome of the execution.
        // TODO: better return type
        send_back: oneshot::Sender<Result<ExecuteSuccess, ()>>,
    },
}

pub struct ExecuteSuccess {
    /// The block that was passed as parameter.
    pub block: block::Block,
    pub storage_changes: HashMap<Vec<u8>, Option<Vec<u8>>>,
}

/// Configuration for that task.
pub struct Config {
    /// Access to all the data of the blockchain.
    pub storage: storage::Storage,
    /// How to spawn other background tasks.
    pub tasks_executor: Box<dyn Fn(Pin<Box<dyn Future<Output = ()> + Send>>) + Send>,
    /// Receiver for messages that the executor task will process.
    pub to_executor: mpsc::Receiver<ToExecutor>,
}

/// Runs the task itself.
pub async fn run_executor_task(mut config: Config) {
    // Tuple of the runtime code of the chain head and its corresponding `WasmBlob`.
    // Used to avoid recompiling it every single time.
    let mut wasm_blob_cache: Option<(Vec<u8>, executor::WasmBlob)> = None;

    while let Some(event) = config.to_executor.next().await {
        match event {
            ToExecutor::Execute {
                mut to_execute,
                send_back,
            } => {
                if send_back.is_canceled() {
                    continue;
                }

                let parent = config
                    .storage
                    .block(&to_execute.header.parent_hash)
                    .storage()
                    .unwrap();

                // In order to avoid parsing/compiling the runtime code every single time, we
                // maintain a cache of the `WasmBlob` of the head of the chain.
                let runtime_wasm_blob = {
                    let code = parent.code_key().unwrap();
                    if wasm_blob_cache
                        .as_ref()
                        .map(|(c, _)| *c != code.as_ref())
                        .unwrap_or(true)
                    {
                        let wasm_blob = executor::WasmBlob::from_bytes(code.as_ref()).unwrap();
                        wasm_blob_cache = Some((code.as_ref().to_vec(), wasm_blob));
                    }
                    &wasm_blob_cache.as_ref().unwrap().1
                };

                let import_result =
                    crate::block_import::verify_block(crate::block_import::Config {
                        runtime: runtime_wasm_blob,
                        block_header: &to_execute.header,
                        block_body: &to_execute.extrinsics,
                        parent_storage_get: {
                            let parent = parent.clone();
                            move |key: Vec<u8>| {
                                let ret: Option<Vec<u8>> =
                                    parent.get(&key).map(|v| v.as_ref().to_vec());
                                async move { ret }
                            }
                        },
                        parent_storage_keys_prefix: {
                            let parent = parent.clone();
                            move |prefix: Vec<u8>| {
                                assert!(prefix.is_empty()); // TODO: not implemented
                                let ret: Vec<Vec<u8>> =
                                    parent.storage_keys().map(|v| v.as_ref().to_vec()).collect();
                                async move { ret }
                            }
                        },
                        parent_storage_next_key: {
                            let parent = parent.clone();
                            move |key: Vec<u8>| {
                                let ret: Option<Vec<u8>> =
                                    parent.next_key(&key).map(|v| v.as_ref().to_vec());
                                async move { ret }
                            }
                        },
                    })
                    .await;

                match import_result {
                    Ok(success) => {
                        if success.storage_top_trie_changes.contains_key(&b":code"[..]) {
                            wasm_blob_cache = None;
                        }

                        let mut new_block_storage = (*parent).clone();
                        for (key, value) in success.storage_top_trie_changes.iter() {
                            if let Some(value) = value.as_ref() {
                                new_block_storage.insert(key, value.clone())
                            } else {
                                new_block_storage.remove(key);
                            }
                        }
                        let new_hash = to_execute.header.block_hash();
                        // TODO: hack because our storage story is bad regarding memory
                        config
                            .storage
                            .remove_storage(&to_execute.header.parent_hash);
                        config
                            .storage
                            .block(&new_hash.0.into())
                            .set_storage(new_block_storage);

                        let _ = send_back.send(Ok(ExecuteSuccess {
                            block: to_execute,
                            storage_changes: success.storage_top_trie_changes,
                        }));
                    }
                    Err(_) => panic!(), // TODO:
                }
            }
        }
    }
}