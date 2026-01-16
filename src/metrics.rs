// metrics.rs

use crate::types::BotMetrics;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::RwLock;
use warp::Filter;
use serde_json::json;
use tracing::info;

pub struct MetricsServer {
    port: u16,
    metrics: Arc<RwLock<BotMetrics>>,
}

impl MetricsServer {
    pub fn new(port: u16, metrics: Arc<RwLock<BotMetrics>>) -> Self {
        Self { port, metrics }
    }

    pub async fn run(self) -> Result<()> {
        let metrics = self.metrics.clone();

        // Route: /metrics (Prometheus format)
        let metrics_route = warp::path("metrics")
            .and(warp::get())
            .and(with_metrics(metrics.clone()))
            .and_then(handle_metrics);

        // Route: /stats (JSON format)
        let stats_route = warp::path("stats")
            .and(warp::get())
            .and(with_metrics(metrics.clone()))
            .and_then(handle_stats);

        // Route: /health
        let health_route = warp::path("health")
            .and(warp::get())
            .and(with_metrics(metrics.clone()))
            .and_then(handle_health);

        // Route: /reset (POST)
        let reset_route = warp::path("reset")
            .and(warp::post())
            .and(with_metrics(metrics.clone()))
            .and_then(handle_reset);

        // Route: /dashboard
        let dashboard_route = warp::path("dashboard")
            .and(warp::get())
            .and(with_metrics(metrics.clone()))
            .and_then(handle_dashboard);
        

        let routes = metrics_route
            .or(stats_route)
            .or(health_route)
            .or(reset_route)
            .or(dashboard_route);

        info!("📊 Metrics server listening on http://0.0.0.0:{}", self.port);

        warp::serve(routes)
            .run(([0, 0, 0, 0], self.port))
            .await;

        Ok(())
    }
}

fn with_metrics(
    metrics: Arc<RwLock<BotMetrics>>,
) -> impl Filter<Extract = (Arc<RwLock<BotMetrics>>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || metrics.clone())
}

// ========== HANDLER: PROMETHEUS METRICS ==========

async fn handle_metrics(
    metrics: Arc<RwLock<BotMetrics>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let m = metrics.read().await;

    let prometheus_format = format!(
        r#"# HELP arbitrage_bot_trades_total Total number of trades executed
# TYPE arbitrage_bot_trades_total counter
arbitrage_bot_trades_total {{status="total"}} {}
arbitrage_bot_trades_total {{status="successful"}} {}
arbitrage_bot_trades_total {{status="failed"}} {}

# HELP arbitrage_bot_profit_usd Total profit in USD
# TYPE arbitrage_bot_profit_usd gauge
arbitrage_bot_profit_usd {{type="gross"}} {}
arbitrage_bot_profit_usd {{type="gas_cost"}} {}
arbitrage_bot_profit_usd {{type="net"}} {}

# HELP arbitrage_bot_success_rate Success rate percentage
# TYPE arbitrage_bot_success_rate gauge
arbitrage_bot_success_rate {}

# HELP arbitrage_bot_avg_profit_per_trade Average profit per trade in USD
# TYPE arbitrage_bot_avg_profit_per_trade gauge
arbitrage_bot_avg_profit_per_trade {}

# HELP arbitrage_bot_opportunities_found Total opportunities found
# TYPE arbitrage_bot_opportunities_found counter
arbitrage_bot_opportunities_found {}

# HELP arbitrage_bot_opportunities_executed Total opportunities executed
# TYPE arbitrage_bot_opportunities_executed counter
arbitrage_bot_opportunities_executed {}

# HELP arbitrage_bot_execution_rate Execution rate percentage
# TYPE arbitrage_bot_execution_rate gauge
arbitrage_bot_execution_rate {}

# HELP arbitrage_bot_uptime_seconds Bot uptime in seconds
# TYPE arbitrage_bot_uptime_seconds counter
arbitrage_bot_uptime_seconds {}
"#,
        m.total_trades,
        m.successful_trades,
        m.failed_trades,
        m.total_profit_usd,
        m.total_gas_cost_usd,
        m.total_profit_usd - m.total_gas_cost_usd,
        m.success_rate(),
        m.average_profit_per_trade,
        m.opportunities_found,
        m.opportunities_executed,
        if m.opportunities_found > 0 {
            (m.opportunities_executed as f64 / m.opportunities_found as f64) * 100.0
        } else {
            0.0
        },
        m.uptime_seconds,
    );

    Ok(warp::reply::with_header(
        prometheus_format,
        "Content-Type",
        "text/plain; version=0.0.4",
    ))
}

// ========== HANDLER: JSON STATS ==========

async fn handle_stats(
    metrics: Arc<RwLock<BotMetrics>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let m = metrics.read().await;

    let stats = json!({
        "trades": {
            "total": m.total_trades,
            "successful": m.successful_trades,
            "failed": m.failed_trades,
            "success_rate": format!("{:.2}%", m.success_rate())
        },
        "profit": {
            "total_usd": format!("{:.2}", m.total_profit_usd),
            "gas_cost_usd": format!("{:.2}", m.total_gas_cost_usd),
            "net_usd": format!("{:.2}", m.total_profit_usd - m.total_gas_cost_usd),
            "average_per_trade": format!("{:.2}", m.average_profit_per_trade)
        },
        "opportunities": {
            "found": m.opportunities_found,
            "executed": m.opportunities_executed,
            "execution_rate": format!("{:.2}%",
                if m.opportunities_found > 0 {
                    (m.opportunities_executed as f64 / m.opportunities_found as f64) * 100.0
                } else {
                    0.0
                }
            )
        },
        "uptime": {
            "seconds": m.uptime_seconds,
            "hours": format!("{:.2}", m.uptime_seconds as f64 / 3600.0),
            "days": format!("{:.2}", m.uptime_seconds as f64 / 86400.0)
        }
    });

    Ok(warp::reply::json(&stats))
}


async fn handle_dashboard(
    metrics: Arc<RwLock<BotMetrics>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let m = metrics.read().await;
    
    let html = format!(r#"
<!DOCTYPE html>
<html>
<head>
    <title>Arbitrage Bot Dashboard</title>
    <meta http-equiv="refresh" content="5">
    <style>
        body {{
            font-family: 'Courier New', monospace;
            background: #1a1a1a;
            color: #00ff00;
            padding: 20px;
        }}
        .metric {{
            background: #0a0a0a;
            border: 1px solid #00ff00;
            padding: 15px;
            margin: 10px 0;
            border-radius: 5px;
        }}
        .good {{ color: #00ff00; }}
        .warn {{ color: #ffaa00; }}
        .bad {{ color: #ff0000; }}
        h1 {{ color: #00ffff; }}
        .grid {{
            display: grid;
            grid-template-columns: repeat(3, 1fr);
            gap: 15px;
        }}
    </style>
</head>
<body>
    <h1>🤖 Arbitrage Bot Dashboard</h1>
    
    <div class="grid">
        <div class="metric">
            <h3>💰 Profit</h3>
            <div class="good">Total: ${:.2}</div>
            <div>24h: ${:.2}</div>
            <div>Avg/trade: ${:.2}</div>
        </div>
        
        <div class="metric">
            <h3>📊 Trades</h3>
            <div>Total: {}</div>
            <div class="good">Success: {} ({:.1}%)</div>
            <div class="{}">Failed: {}</div>
        </div>
        
        <div class="metric">
            <h3>🎯 Execution</h3>
            <div>Found: {}</div>
            <div>Executed: {}</div>
            <div>Rate: {:.1}%</div>
        </div>
    </div>
    
    <p style="color: #666;">Auto-refresh every 5s | Uptime: {:.1}h</p>
</body>
</html>
    "#,
        m.total_profit_usd,
        m.total_gas_cost_usd,
        m.total_profit_usd - m.total_gas_cost_usd,
        m.total_trades,
        m.successful_trades,
        m.success_rate(),
        if m.failed_trades > 10 { "warn" } else { "good" },
        m.failed_trades,
        m.opportunities_found,
        m.opportunities_executed,
        if m.opportunities_found > 0 {
            (m.opportunities_executed as f64 / m.opportunities_found as f64) * 100.0
        } else {
            0.0
        },
        m.uptime_seconds as f64 / 3600.0
    );
    
    Ok(warp::reply::html(html))
}

// ========== HANDLER: HEALTH CHECK ==========

async fn handle_health(
    metrics: Arc<RwLock<BotMetrics>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let m = metrics.read().await;

    // Critères de santé
    let is_healthy = m.success_rate() > 50.0 || m.total_trades < 10;
    let recent_activity = m.uptime_seconds > 0;

    let status = if is_healthy && recent_activity {
        "healthy"
    } else if !recent_activity {
        "starting"
    } else {
        "degraded"
    };

    let health = json!({
        "status": status,
        "uptime_seconds": m.uptime_seconds,
        "total_trades": m.total_trades,
        "success_rate": m.success_rate(),
        "checks": {
            "success_rate_ok": m.success_rate() > 50.0 || m.total_trades < 10,
            "recent_activity": recent_activity
        }
    });

    let status_code = if status == "healthy" {
        warp::http::StatusCode::OK
    } else {
        warp::http::StatusCode::SERVICE_UNAVAILABLE
    };

    Ok(warp::reply::with_status(
        warp::reply::json(&health),
        status_code,
    ))
}

// ========== HANDLER: RESET STATS ==========

async fn handle_reset(
    metrics: Arc<RwLock<BotMetrics>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let mut m = metrics.write().await;

    let old_stats = m.clone();

    // Reset tout sauf uptime
    m.total_trades = 0;
    m.successful_trades = 0;
    m.failed_trades = 0;
    m.total_profit_usd = 0.0;
    m.total_gas_cost_usd = 0.0;
    m.average_profit_per_trade = 0.0;
    m.opportunities_found = 0;
    m.opportunities_executed = 0;

    info!("🔄 Metrics reset via API");

    let response = json!({
        "status": "success",
        "message": "Metrics reset successfully",
        "previous_stats": {
            "total_trades": old_stats.total_trades,
            "total_profit_usd": old_stats.total_profit_usd
        }
    });

    Ok(warp::reply::json(&response))
}

// ========== TESTS ==========

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_metrics_format() {
        let metrics = Arc::new(RwLock::new(BotMetrics {
            total_trades: 10,
            successful_trades: 8,
            failed_trades: 2,
            total_profit_usd: 100.0,
            total_gas_cost_usd: 10.0,
            average_profit_per_trade: 12.5,
            opportunities_found: 20,
            opportunities_executed: 10,
            uptime_seconds: 3600,
        }));

        // Test que les métriques sont bien formatées
        let m = metrics.read().await;
        assert_eq!(m.success_rate(), 80.0);
        assert_eq!(m.total_profit_usd - m.total_gas_cost_usd, 90.0);
    }
}
