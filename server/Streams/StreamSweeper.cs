using Vorcall.Server.Attachments;

namespace Vorcall.Server.Streams;

// Offers nothing ever linked are the one kind of streamed file no other path removes: a client
// that offers a file and then abandons the composer leaves a row behind. Deleting a message takes
// its streamed files with it, so this only ever sees the unlinked ones, on the attachment
// sweeper's schedule and cutoff; there is no file to delete, so a sweep is rows alone.
public sealed class StreamSweeper(StreamDirectory streams, ILogger<StreamSweeper> logger) : BackgroundService
{
    // Late enough that a restart does not compete with the migration and the first connections.
    private static readonly TimeSpan FirstSweepDelay = TimeSpan.FromMinutes(1);

    private static readonly TimeSpan SweepInterval = TimeSpan.FromMinutes(10);

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        try
        {
            await Task.Delay(FirstSweepDelay, stoppingToken);

            while (!stoppingToken.IsCancellationRequested)
            {
                try
                {
                    var removed = await streams.SweepUnlinkedAsync(DateTime.UtcNow - AttachmentsOptions.UnlinkedTtl, stoppingToken);
                    if (removed > 0)
                    {
                        logger.LogInformation("Swept {Count} unlinked streamed files", removed);
                    }
                    else
                    {
                        logger.LogDebug("Swept no unlinked streamed files");
                    }
                }
                catch (Exception ex) when (ex is not OperationCanceledException)
                {
                    // A sweep that fails leaves rows to reclaim ten minutes later; throwing out of
                    // ExecuteAsync would instead take the host down with it.
                    logger.LogError(ex, "Streamed file sweep failed");
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
