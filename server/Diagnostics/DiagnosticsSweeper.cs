namespace Vorcall.Server.Diagnostics;

// Uploaded reports are files nothing else ever deletes: without this, a directory the owner
// reads by hand grows until the disk says no.
public sealed class DiagnosticsSweeper(DiagnosticsStore store, ILogger<DiagnosticsSweeper> logger) : BackgroundService
{
    // Late enough that a restart does not compete with the migration and the first connections.
    private static readonly TimeSpan FirstSweepDelay = TimeSpan.FromMinutes(5);

    private static readonly TimeSpan SweepInterval = TimeSpan.FromHours(6);

    // Files, not rows: the cutoff is the last write, so a report is kept for its full retention
    // from the moment it landed.
    public int Sweep(DateTime nowUtc)
    {
        var cutoff = nowUtc - DiagnosticsOptions.Retention;
        var removed = 0;
        foreach (var file in store.Files())
        {
            if (file.LastWriteTimeUtc >= cutoff)
            {
                continue;
            }

            try
            {
                file.Delete();
                removed++;
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
            {
                logger.LogDebug("Could not delete diagnostics file {Path} ({Reason})", file.Name, ex.GetType().Name);
            }
        }

        return removed;
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
                    var removed = Sweep(DateTime.UtcNow);
                    if (removed > 0)
                    {
                        logger.LogInformation("Swept {Count} diagnostics reports", removed);
                    }
                    else
                    {
                        logger.LogDebug("Swept no diagnostics reports");
                    }
                }
                catch (Exception ex) when (ex is not OperationCanceledException)
                {
                    // A sweep that fails leaves disk to reclaim six hours later; throwing out of
                    // ExecuteAsync would instead take the host down with it.
                    logger.LogError(ex, "Diagnostics sweep failed");
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
