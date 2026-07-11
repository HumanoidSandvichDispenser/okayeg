//! A dial to an unreachable peer must time out as `Unreachable`, not hang.

use std::time::{Duration, Instant};

use okayeg::Doc;
use okayeg_net::{Error, Node};

#[tokio::test(flavor = "multi_thread")]
async fn dial_to_unknown_peer_times_out() {
    let node = Node::bind().await.expect("bind");
    // A fresh throwaway node's id: nothing is serving it, and with no address
    // info iroh cannot even find it, so the connect can only ever time out.
    let unreachable = Node::bind().await.expect("bind peer").id();
    let doc = Doc::new();

    let started = Instant::now();
    let result = node
        .sync_with(unreachable, &doc, &Default::default(), Duration::from_millis(500))
        .await;
    let elapsed = started.elapsed();

    assert!(
        matches!(result, Err(Error::Unreachable(_))),
        "expected Unreachable, got {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "dial should give up near the 500ms deadline, took {elapsed:?}"
    );
}
