using System.Threading.RateLimiting;
using Microsoft.AspNetCore.RateLimiting;
using Vorcall.Server.Api;
using Vorcall.Server.Auth;

namespace Vorcall.Server.Diagnostics;

// Everything the diagnostics upload needs, kept out of ServiceSetup so the store, the sweepers
// and the policy that limits the endpoint live next to each other. The policy is added through
// Configure<RateLimiterOptions> rather than AddRateLimiter, which ServiceSetup already owns:
// both configurations run, each contributing its own policies.
public static class DiagnosticsSetup
{
    public static void Configure(WebApplicationBuilder builder)
    {
        // An unusable per-hour limit fails the boot here, like the other Vorcall options.
        var options = DiagnosticsOptions.FromConfiguration(builder.Configuration);

        builder.Services.AddSingleton(options);
        builder.Services.AddSingleton<DiagnosticsStore>();
        builder.Services.AddHostedService<DiagnosticsSweeper>();
        builder.Services.AddHostedService<RefreshTokenSweeper>();

        // Partitioned by the bearer's user id: a report is an account's, not an address's.
        builder.Services.Configure<RateLimiterOptions>(limiter =>
            limiter.AddPolicy(DiagnosticsEndpoints.RateLimitPolicy, context =>
                RateLimitPartition.GetSlidingWindowLimiter(
                    BearerIdentity.TryGetUserId(context.User, out var userId)
                        ? $"user:{userId}"
                        : $"ip:{context.Connection.RemoteIpAddress?.ToString() ?? "unknown"}",
                    _ => new SlidingWindowRateLimiterOptions
                    {
                        PermitLimit = options.ReportsPerHour,
                        Window = TimeSpan.FromHours(1),
                        SegmentsPerWindow = 6,
                        QueueLimit = 0,
                    })));
    }
}
