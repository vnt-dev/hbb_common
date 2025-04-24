use crate::punch::PunchContext;
use async_shutdown::ShutdownManager;
use rust_p2p_core::idle::IdleRouteManager;
use rust_p2p_core::route::route_table::RouteTable;
use rust_p2p_core::tunnel::SocketManager;
use std::sync::Arc;
use tokio::task::JoinSet;

mod heartbeat;
mod idle;
mod nat_query;
mod query_public_addr;
pub fn start_task(
    self_id: Arc<String>,
    idle_route_manager: IdleRouteManager<Arc<String>>,
    shutdown_manager: ShutdownManager<()>,
    route_table: RouteTable<Arc<String>>,
    punch_context: Arc<PunchContext>,
    socket_manager: SocketManager,
) {
    let mut join_set = JoinSet::new();
    join_set.spawn(nat_query::nat_test_loop(
        punch_context.clone(),
        socket_manager.clone(),
    ));
    join_set.spawn(query_public_addr::query_tcp_public_addr_loop(
        punch_context.clone(),
        socket_manager.clone(),
    ));
    join_set.spawn(query_public_addr::query_udp_public_addr_loop(
        punch_context.clone(),
        socket_manager.clone(),
    ));
    join_set.spawn(heartbeat::heartbeat_loop(
        self_id,
        route_table,
        socket_manager.clone(),
    ));
    join_set.spawn(idle::idle_check_loop(idle_route_manager));
    let mut join_set = join_set;
    let fut =
        shutdown_manager.wrap_cancel(async move { while join_set.join_next().await.is_some() {} });
    tokio::spawn(async move {
        if fut.await.is_err() {
            log::debug!("recv shutdown signal: built-in maintain tasks are shutdown");
        }
    });
}
