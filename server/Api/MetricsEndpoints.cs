using System.Globalization;
using System.Text;
using Vorcall.Server.Chat;
using Vorcall.Server.Metrics;
using Vorcall.Server.Voice;

namespace Vorcall.Server.Api;

// The Prometheus text exposition, written by hand: it is a few dozen lines of formatting, and a
// client library would be a dependency that buys nothing. Deliberately outside /api, so the
// pre-shared key middleware does not gate it and a scraper on the host needs no door key — what
// gates it is where the request came from.
public static class MetricsEndpoints
{
    private const string ExpositionContentType = "text/plain; version=0.0.4; charset=utf-8";

    public static void Map(WebApplication app)
    {
        app.MapGet("/metrics", (HttpContext context, ConnectionRegistry registry, VoiceRelay relay, ServerMetrics metrics) =>
            PrivateSource.IsPrivate(context)
                ? Results.Text(Render(registry.Snapshot(), relay.Snapshot(), metrics), ExpositionContentType)

                // The same answer as an unknown path: a scan from outside does not get to learn
                // that this server has a metrics endpoint at all.
                : Results.NotFound());
    }

    private static string Render(RegistrySnapshot registry, RelaySnapshot relay, ServerMetrics metrics)
    {
        var body = new StringBuilder();

        Declare(body, "vorcall_connections", "gauge", "WebSocket connections currently accepted.");
        Sample(body, "vorcall_connections", registry.Connections);

        Declare(body, "vorcall_online_users", "gauge", "Accounts with a live connection.");
        Sample(body, "vorcall_online_users", registry.OnlineUsers);

        Declare(body, "vorcall_channels", "gauge", "Channels the mirror holds, DMs included.");
        Sample(body, "vorcall_channels", registry.Channels);

        Declare(body, "vorcall_voice_sessions", "gauge", "Voice slots held across all channels.");
        Sample(body, "vorcall_voice_sessions", registry.VoiceSessions);

        Declare(body, "vorcall_relay_sessions", "gauge", "Sessions the media relay holds a key for.");
        Sample(body, "vorcall_relay_sessions", relay.Sessions);

        Declare(body, "vorcall_sharers", "gauge", "Voice slots currently sharing a screen.");
        Sample(body, "vorcall_sharers", registry.Sharers);

        Declare(body, "vorcall_watchers", "gauge", "Voice slots currently watching a share.");
        Sample(body, "vorcall_watchers", registry.Watchers);

        Declare(body, "vorcall_uptime_seconds", "gauge", "Seconds since this process built its metrics.");
        body.Append("vorcall_uptime_seconds ")
            .Append((DateTime.UtcNow - metrics.StartedAt).TotalSeconds.ToString("0.000", CultureInfo.InvariantCulture))
            .Append('\n');

        Declare(body, "vorcall_ws_connections_total", "counter", "WebSocket upgrades accepted.");
        Sample(body, "vorcall_ws_connections_total", metrics.WsAcceptedTotal);

        Declare(body, "vorcall_messages_total", "counter", "Chat messages appended.");
        Sample(body, "vorcall_messages_total", metrics.MessagesTotal);

        Declare(body, "vorcall_uploads_total", "counter", "Attachments stored.");
        Sample(body, "vorcall_uploads_total", metrics.UploadsTotal);

        Declare(body, "vorcall_http_responses_total", "counter", "HTTP responses by status class.");
        Sample(body, "vorcall_http_responses_total", "class=\"2xx\"", metrics.Http2xx);
        Sample(body, "vorcall_http_responses_total", "class=\"4xx\"", metrics.Http4xx);
        Sample(body, "vorcall_http_responses_total", "class=\"5xx\"", metrics.Http5xx);

        Declare(body, "vorcall_rate_limited_total", "counter", "Requests and frames a rate limiter refused.");
        Sample(body, "vorcall_rate_limited_total", "kind=\"http\"", metrics.HttpRateLimitedTotal);
        Sample(body, "vorcall_rate_limited_total", "kind=\"message\"", Interlocked.Read(ref WriteLimiter.RejectedTotal));

        Declare(body, "vorcall_relay_packets_total", "counter", "Media datagrams the relay took in and sent on.");
        Sample(body, "vorcall_relay_packets_total", "direction=\"in\",kind=\"audio\"", relay.PacketsIn);
        Sample(body, "vorcall_relay_packets_total", "direction=\"out\",kind=\"audio\"", relay.PacketsOut);
        Sample(body, "vorcall_relay_packets_total", "direction=\"in\",kind=\"share\"", relay.SharePacketsIn);
        Sample(body, "vorcall_relay_packets_total", "direction=\"out\",kind=\"share\"", relay.SharePacketsOut);

        Declare(body, "vorcall_relay_bytes_total", "counter", "Media bytes the relay took in and sent on.");
        Sample(body, "vorcall_relay_bytes_total", "direction=\"in\",kind=\"audio\"", relay.BytesIn);
        Sample(body, "vorcall_relay_bytes_total", "direction=\"out\",kind=\"audio\"", relay.BytesOut);
        Sample(body, "vorcall_relay_bytes_total", "direction=\"in\",kind=\"share\"", relay.ShareBytesIn);
        Sample(body, "vorcall_relay_bytes_total", "direction=\"out\",kind=\"share\"", relay.ShareBytesOut);

        Declare(body, "vorcall_relay_keyframe_requests_total", "counter", "Keyframe requests forwarded to a sharer.");
        Sample(body, "vorcall_relay_keyframe_requests_total", relay.KeyframeRequests);

        Declare(body, "vorcall_relay_drops_total", "counter", "Media datagrams the relay refused, by reason.");
        foreach (var (reason, count) in relay.Drops)
        {
            Sample(body, "vorcall_relay_drops_total", $"reason=\"{reason}\"", count);
        }

        return body.ToString();
    }

    private static void Declare(StringBuilder body, string name, string type, string help)
        => body.Append("# HELP ").Append(name).Append(' ').Append(help).Append('\n')
            .Append("# TYPE ").Append(name).Append(' ').Append(type).Append('\n');

    private static void Sample(StringBuilder body, string name, long value)
        => body.Append(name).Append(' ').Append(value.ToString(CultureInfo.InvariantCulture)).Append('\n');

    private static void Sample(StringBuilder body, string name, string labels, long value)
        => body.Append(name).Append('{').Append(labels).Append("} ")
            .Append(value.ToString(CultureInfo.InvariantCulture)).Append('\n');
}
