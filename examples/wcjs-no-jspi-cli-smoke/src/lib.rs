mod bindings {
    wit_bindgen::generate!({
        inline: "
            package witbindgen:smoke;

            world smoke {
                include wasi:cli/imports@0.3.0-rc-2026-02-09;
                export wasi:cli/run@0.3.0-rc-2026-02-09;
            }
        ",
        path: "wit",
        world: "witbindgen:smoke/smoke",
        generate_all,
    });
    use super::Component;
    export!(Component);
}

use bindings::exports::wasi::cli::run::Guest;
use bindings::wit_stream;
use bindings::wasi::clocks::monotonic_clock;

struct Component;

impl Guest for Component {
    async fn run() -> Result<(), ()> {
        let (tx, rx) = wit_stream::new();
        futures::join!(
            async {
                bindings::wasi::cli::stdout::write_via_stream(rx).await.unwrap();
            },
            async {
                let (tx, _status, _buf) = tx.write(b"before sleep\n".to_vec()).await;
                let start = monotonic_clock::now();
                monotonic_clock::wait_for(10_000_000).await;
                let elapsed = monotonic_clock::now() - start;
                let msg = format!("after sleep ({elapsed} ns)\n");
                let (tx, _status, _buf) = tx.write(msg.into_bytes()).await;
                drop(tx);
            },
        );
        Ok(())
    }
}
