//@ wasmtime-flags = '-Wcomponent-model-async'

include!(env!("BINDINGS"));

use crate::a::b::the_test::f;

struct Component;

export!(Component);

impl Guest for Component {
    async fn run() {
        let (tx, rx) = wit_future::new(|| unreachable!());

        let a = tx.write(());
        let b = f(rx);
        let (a_result, ()) = futures::join!(a, b);
        a_result.unwrap();
    }
}
