#![cfg(feature = "discovery")]

//! This module has a test to run manually that can be tested against the
//! [Counter example](https://github.com/restatedev/sdk-java/blob/main/examples/src/main/java/dev/restate/sdk/examples/BlockingCounter.java) in sdk-java.

use common::retry_policy::RetryPolicy;
use hyper::Uri;
use service_protocol::discovery::*;
use test_utils::test;

#[ignore]
#[test(tokio::test)]
async fn counter_discovery() {
    let discovery = ServiceDiscovery::new(RetryPolicy::None);

    let discovered_metadata = discovery
        .discover(
            &Uri::from_static("http://localhost:8080"),
            &Default::default(),
        )
        .await
        .unwrap();

    println!("{:#?}", discovered_metadata);
}