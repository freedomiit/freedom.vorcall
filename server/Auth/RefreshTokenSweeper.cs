using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Data;

namespace Vorcall.Server.Auth;

// Rotation writes a new refresh-token row per login and per refresh, and nothing has ever
// deleted the old ones. A row kept past its expiry or revocation proves nothing: replay is
// already refused by the timestamps, so the grace period only exists so a support question
// about last week still has something to look at.
public sealed class RefreshTokenSweeper(
    IDbContextFactory<AppDbContext> contexts,
    ILogger<RefreshTokenSweeper> logger) : BackgroundService
{
    public static readonly TimeSpan Grace = TimeSpan.FromDays(7);

    private static readonly TimeSpan FirstSweepDelay = TimeSpan.FromMinutes(2);

    private static readonly TimeSpan SweepInterval = TimeSpan.FromDays(1);

    public async Task<int> SweepAsync(DateTime nowUtc, CancellationToken ct = default)
    {
        var cutoff = nowUtc - Grace;
        await using var db = await contexts.CreateDbContextAsync(ct);
        return await db.RefreshTokens
            .Where(t => t.ExpiresAt < cutoff || (t.RevokedAt != null && t.RevokedAt < cutoff))
            .ExecuteDeleteAsync(ct);
    }

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        try
        {
            await Task.Delay(FirstSweepDelay, stoppingToken);

            while (!stoppingToken.IsCancellationRequested)
            {
                try
                {
                    var removed = await SweepAsync(DateTime.UtcNow, stoppingToken);
                    if (removed > 0)
                    {
                        logger.LogInformation("Swept {Count} expired refresh tokens", removed);
                    }
                    else
                    {
                        logger.LogDebug("Swept no expired refresh tokens");
                    }
                }
                catch (Exception ex) when (ex is not OperationCanceledException)
                {
                    // A database that is down now will be up tomorrow; throwing out of
                    // ExecuteAsync would instead take the host down with it.
                    logger.LogError(ex, "Refresh token sweep failed");
                }

                await Task.Delay(SweepInterval, stoppingToken);
            }
        }
        catch (OperationCanceledException)
        {
            // Shutdown.
        }
    }
}
