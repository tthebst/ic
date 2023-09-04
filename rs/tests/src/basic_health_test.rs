/* tag::catalog[]
Title:: Basic system health test

Goal:: Start an IC with 2 subnets, 4 nodes per subnet. Install canisters and
make update and query calls that require cross-subnet communication to
succeed. While this is happening, ensure the IC finalizes rounds correctly
across all nodes in each subnet. No real load is generated by this test, it's
primarily a smoke test to ensure that the IC can be started and that our basic
system testing facilities are working as expected.

Runbook::
. Set up two subnets with four nodes each
. Install a universal canister in both
. Verify that the canisters can be queried
. Verify that the canisters can be updated and the modifications queried
. Perform cross-net messaging from each UC to the other
. Verify that the canisters' state differs in-between
. Verify that the canisters finally arrive at an equal state
. Verify that all the nodes self-report as healthy.

Success:: All mutations to the subnets and installed canisters on them occur
in the expected way. Intermediate and final canister states can be observed.
All system health checks, as detected by `ekg::basic_monitoring`, must pass.

Coverage::
. Root and secondary subnets can be created
. Canisters can be installed regardless of subnet
. Canisters can be queried and observably updated regardless of subnet
. Cross-subnet updates are possible


end::catalog[] */

use std::time::Duration;

use crate::driver::ic::{InternetComputer, Subnet};
use crate::driver::prometheus_vm::{HasPrometheus, PrometheusVm};
use crate::driver::test_env::TestEnv;
use crate::driver::test_env_api::*;
use crate::util::*; // to use the universal canister
use anyhow::bail;
use ic_registry_subnet_type::SubnetType;
use slog::info;

pub fn config_single_host(env: TestEnv) {
    PrometheusVm::default()
        .start(&env)
        .expect("failed to start prometheus VM");
    InternetComputer::new()
        .add_subnet(Subnet::new(SubnetType::System).add_nodes(4))
        .add_subnet(Subnet::new(SubnetType::Application).add_nodes(4))
        .setup_and_start(&env)
        .expect("failed to setup IC under test");
    env.sync_with_prometheus();
}

const MSG: &[u8] = b"this beautiful prose should be persisted for future generations";
const READ_RETRIES: u64 = 10;
const RETRY_WAIT: Duration = Duration::from_secs(10);

/// Here we define the test workflow, which should implement the Runbook given
/// in the test catalog entry at the top of this file.
pub fn test(env: TestEnv) {
    let log = env.logger();
    // Assemble a list that contains one node per subnet.
    let nodes: Vec<_> = env
        .topology_snapshot()
        .subnets()
        .map(|s| s.nodes().next().unwrap())
        .collect();

    info!(log, "Waiting for the nodes to become healthy ...");
    nodes
        .iter()
        .try_for_each(|n| n.await_status_is_healthy())
        .unwrap();

    info!(log, "Installing universal canisters on subnets (via all nodes), reading and storing messages ...");
    let ucan_ids: Vec<_> = nodes
        .iter()
        .map(|node| {
            let inner_log = log.clone();
            let effective_canister_id = node.effective_canister_id();
            node.with_default_agent(move |agent| async move {
                let ucan =
                    UniversalCanister::new_with_retries(&agent, effective_canister_id, &inner_log)
                        .await;

                // send a query call to it
                assert_eq!(ucan.try_read_stable(0, 0).await, Vec::<u8>::new());

                // send an update call to it
                ucan.store_to_stable(0, MSG).await;

                // query for mutated data
                assert_eq!(
                    ucan.try_read_stable(0, MSG.len() as u32).await,
                    MSG.to_vec()
                );

                ucan.canister_id()
            })
        })
        .collect();

    // Match up canisters with each other. The first canister of the pair will
    // send an update to the second canister of the pair (see below).
    let canister_info = ucan_ids
        .clone()
        .into_iter()
        .zip(ucan_ids.clone().into_iter().rev())
        .collect::<Vec<_>>();
    const XNET_MSG: &[u8] = b"just received a xnet message";

    // We expect to find these contents in stable memory.
    let expected_memory_values = vec![MSG, XNET_MSG].into_iter().map(|s| s.to_vec());

    info!(log, "Sending xnet messages ...");
    // Again we execute functions to call each of the canisters on the
    // subnets, making sure that the memory contents we expect to see are
    // indeed set to the updated value. We want until all have succeeded.
    // Since interactions with the universal canister are `async`, we must
    // execute these within the context of the Tokio runtime, even though
    // there is no concurrency from this point forward.
    for ((n, (from, to)), expect) in nodes.iter().zip(canister_info).zip(expected_memory_values) {
        let log = log.clone();
        n.with_default_agent(move |agent| async move {
            // Note: `from` is the canister id of the univeral canister that was
            // installed on `from`.
            info!(log, "Initializing universal canister...");
            let ucan = UniversalCanister::from_canister_id(&agent, from);

            // Send a cross-subnet update message.
            info!(log, "Sending update message to the universal canister...");
            ucan.forward_to(&to, "update", UniversalCanister::stable_writer(0, XNET_MSG))
                .await
                .expect("failed to send update message");

            info!(log, "Assert correct read message from canister...");
            // Verify the originating canister now has the expected content.
            assert_eq!(
                ucan.try_read_stable(0, expect.len() as u32).await,
                expect.to_vec()
            );
        });
    }

    info!(log, "Assert that message has been stored ...");
    // Finally we query each of the canisters to ensure that the canister
    // memories have been updated as expected.
    for (node, ucan_id) in nodes.iter().zip(ucan_ids) {
        let log = log.clone();
        node.with_default_agent(move |agent| async move {
            let ucan = UniversalCanister::from_canister_id(&agent, ucan_id);
            // NOTE: retries are important here, 1/3 of the nodes might not observe changes immediately.
            retry_async(&log, READY_WAIT_TIMEOUT, RETRY_BACKOFF, || async {
                let current_msg = ucan
                    .try_read_stable_with_retries(
                        &log,
                        0,
                        XNET_MSG.len() as u32,
                        READ_RETRIES,
                        RETRY_WAIT,
                    )
                    .await;
                if current_msg != XNET_MSG.to_vec() {
                    bail!("Expected message not found!")
                }
                Ok(())
            })
            .await
            .expect("Node not healthy");
        })
    }
}
