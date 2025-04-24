pub async fn idle_check_loop(idle_route_manager: rust_p2p_core::idle::IdleRouteManager<String>) {
    loop {
        let (peer_id, route, _) = idle_route_manager.next_idle().await;
        idle_route_manager.remove_route(&peer_id, &route.route_key());
        log::info!("idle {peer_id:?},{route:?}");
    }
}
