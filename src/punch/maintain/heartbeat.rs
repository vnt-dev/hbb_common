use crate::punch::protocol::ping;
use rust_p2p_core::route::route_table::RouteTable;
use rust_p2p_core::tunnel::SocketManager;
use std::time::Duration;

pub async fn heartbeat_loop(
    oneself_id: String,
    route_table: RouteTable<String>,
    socket_manager: SocketManager,
) {
    loop {
        heartbeat(&oneself_id, &route_table, &socket_manager).await;
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

pub async fn heartbeat(
    oneself_id: &String,
    route_table: &RouteTable<String>,
    socket_manager: &SocketManager,
) {
    let table = route_table.route_table();
    for (_peer_id, routes) in table {
        for route in routes {
            if let Ok(packet) = ping(&oneself_id) {
                _ = socket_manager
                    .send_to(packet.into_buffer(), &route.route_key())
                    .await;
            }
        }
    }
}
