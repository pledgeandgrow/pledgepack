//! Per-module plugin hooks (`resolveId` / `load` / `transform`) for the dev
//! server, driven through the same [`PluginHooks`] trait the production build
//! uses, so plugin ordering semantics (first-non-null for `resolveId`/`load`,
//! chained `transform` with composed source maps) are identical in both modes.
//!
//! Plugin runtimes such as QuickJS are single-threaded (`!Send`), but axum
//! handlers run on a multi-threaded runtime. [`PluginHookService`] therefore
//! owns the plugin host on ONE dedicated OS thread (the host is *built* on
//! that thread, so it never has to be `Send`) and exposes a cheap, `Send +
//! Sync` async handle that forwards requests over a channel.

use anyhow::{Result, anyhow};
use pledgepack_core::plugin_hooks::{CodeResult, HookRunner, PluginHooks, ResolvedId};
use std::sync::Arc;
use tokio::sync::oneshot;

type Reply<T> = oneshot::Sender<Result<T>>;

enum Job {
    ResolveId {
        source: String,
        importer: Option<String>,
        reply: Reply<Option<ResolvedId>>,
    },
    Load {
        id: String,
        reply: Reply<Option<CodeResult>>,
    },
    Transform {
        code: String,
        id: String,
        reply: Reply<Option<CodeResult>>,
    },
}

/// `Send + Sync` handle to a plugin host running on its own thread.
pub struct PluginHookService {
    tx: std::sync::mpsc::Sender<Job>,
}

impl PluginHookService {
    /// Start the service. `factory` runs ON the service thread and builds the
    /// plugin host there (so the host may be `!Send`). Returns `None` when the
    /// factory yields no host (e.g. no plugins, or the trust check failed).
    ///
    /// Blocks until the factory has finished so the caller knows whether hooks
    /// are available before it starts serving.
    pub fn spawn<F>(factory: F) -> Option<Arc<Self>>
    where
        F: FnOnce() -> Option<Box<dyn PluginHooks>> + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
        let spawned = std::thread::Builder::new()
            .name("pledge-dev-plugin-hooks".to_string())
            .spawn(move || {
                let Some(hooks) = factory() else {
                    let _ = ready_tx.send(false);
                    return;
                };
                let _ = ready_tx.send(true);
                let runner = HookRunner::new(hooks.as_ref());
                // Ends when every sender (the service handle) is dropped.
                while let Ok(job) = rx.recv() {
                    match job {
                        Job::ResolveId {
                            source,
                            importer,
                            reply,
                        } => {
                            let _ = reply.send(runner.resolve_id(&source, importer.as_deref()));
                        }
                        Job::Load { id, reply } => {
                            let _ = reply.send(runner.load(&id));
                        }
                        Job::Transform { code, id, reply } => {
                            let _ = reply.send(runner.transform(&code, &id));
                        }
                    }
                }
            });
        if spawned.is_err() || !ready_rx.recv().unwrap_or(false) {
            return None;
        }
        Some(Arc::new(Self { tx }))
    }

    async fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Job) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .map_err(|_| anyhow!("plugin host thread has stopped"))?;
        rx.await
            .map_err(|_| anyhow!("plugin host dropped the request"))?
    }

    /// `resolveId`: first plugin returning non-null wins.
    pub async fn resolve_id(
        &self,
        source: &str,
        importer: Option<&str>,
    ) -> Result<Option<ResolvedId>> {
        self.ask(|reply| Job::ResolveId {
            source: source.to_string(),
            importer: importer.map(String::from),
            reply,
        })
        .await
    }

    /// `load`: first plugin returning non-null wins.
    pub async fn load(&self, id: &str) -> Result<Option<CodeResult>> {
        self.ask(|reply| Job::Load {
            id: id.to_string(),
            reply,
        })
        .await
    }

    /// `transform`: chained in plugin order; `None` when no plugin changed
    /// anything. The returned map (if any) is already composed across the chain.
    pub async fn transform(&self, code: &str, id: &str) -> Result<Option<CodeResult>> {
        self.ask(|reply| Job::Transform {
            code: code.to_string(),
            id: id.to_string(),
            reply,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pledgepack_core::plugin_hooks::{BuildHook, HtmlResult};
    use std::cell::Cell;

    /// A deliberately `!Send` host (holds a `Cell`) to prove the service never
    /// requires `Send` of the plugin host.
    struct Host {
        calls: Cell<usize>,
    }

    impl PluginHooks for Host {
        fn plugin_count(&self) -> usize {
            1
        }
        fn plugin_name(&self, _: usize) -> String {
            "dev-test".into()
        }
        fn supports(&self, _: usize, hook: BuildHook) -> bool {
            matches!(
                hook,
                BuildHook::ResolveId | BuildHook::Load | BuildHook::Transform
            )
        }
        fn resolve_id(&self, _: usize, s: &str, _: Option<&str>) -> Result<Option<ResolvedId>> {
            Ok((s == "virtual:x").then(|| ResolvedId {
                id: "\0virtual:x".into(),
                external: false,
            }))
        }
        fn load(&self, _: usize, id: &str) -> Result<Option<CodeResult>> {
            Ok((id == "\0virtual:x").then(|| CodeResult {
                code: "export default 1".into(),
                map: None,
            }))
        }
        fn transform(&self, _: usize, code: &str, _: &str) -> Result<Option<CodeResult>> {
            self.calls.set(self.calls.get() + 1);
            if code.contains("BOOM") {
                anyhow::bail!("boom");
            }
            Ok(code.contains("__A__").then(|| CodeResult {
                code: code.replace("__A__", &self.calls.get().to_string()),
                map: None,
            }))
        }
        fn render_chunk(&self, _: usize, _: &str, _: &str, _: &str) -> Result<Option<CodeResult>> {
            Ok(None)
        }
        fn transform_index_html(&self, _: usize, _: &str, _: &str) -> Result<Option<HtmlResult>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn service_drives_hooks_on_its_own_thread() {
        let svc = PluginHookService::spawn(|| {
            Some(Box::new(Host {
                calls: Cell::new(0),
            }))
        })
        .expect("service starts");

        let r = svc.resolve_id("virtual:x", None).await.unwrap().unwrap();
        assert_eq!(r.id, "\0virtual:x");
        assert!(svc.resolve_id("other", None).await.unwrap().is_none());
        assert_eq!(
            svc.load("\0virtual:x").await.unwrap().unwrap().code,
            "export default 1"
        );
        assert!(svc.load("nope").await.unwrap().is_none());

        // State lives on the service thread (calls counter increments).
        assert_eq!(
            svc.transform("a __A__", "f.ts")
                .await
                .unwrap()
                .unwrap()
                .code,
            "a 1"
        );
        assert!(svc.transform("plain", "f.ts").await.unwrap().is_none());
        assert_eq!(
            svc.transform("__A__", "f.ts").await.unwrap().unwrap().code,
            "3"
        );

        // Plugin errors come back as errors naming the plugin and hook.
        let e = svc.transform("BOOM", "f.ts").await.unwrap_err().to_string();
        assert!(e.contains("dev-test") && e.contains("transform"), "{e}");
    }

    #[test]
    fn no_host_means_no_service() {
        assert!(PluginHookService::spawn(|| None).is_none());
    }
}
