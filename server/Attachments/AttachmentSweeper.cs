namespace Vorcall.Server.Attachments;

// Uploads nothing ever linked are the one kind of attachment no other path deletes: a client
// that picks an image and then abandons the composer leaves a row and a file behind. Deleting a
// message takes its own attachments with it, so this only ever sees the unlinked ones. Images are
// the same story with a different reference — a profile, the server row or a role — and are swept
// on the same schedule.
public sealed class AttachmentSweeper(
    AttachmentStore store,
    ImageStore images,
    ILogger<AttachmentSweeper> logger) : BackgroundService
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
                    var now = DateTime.UtcNow;
                    var removed = await store.SweepUnlinkedAsync(now - AttachmentsOptions.UnlinkedTtl);
                    var removedImages = await images.SweepUnreferencedAsync(
                        AttachmentsOptions.UnlinkedTtl,
                        now,
                        stoppingToken);
                    if (removed > 0 || removedImages > 0)
                    {
                        logger.LogInformation(
                            "Swept {Count} unlinked attachments and {ImageCount} unreferenced images",
                            removed,
                            removedImages);
                    }
                    else
                    {
                        logger.LogDebug("Swept no unlinked attachments and no unreferenced images");
                    }
                }
                catch (Exception ex) when (ex is not OperationCanceledException)
                {
                    // A sweep that fails leaves disk to reclaim ten minutes later; throwing out
                    // of ExecuteAsync would instead take the host down with it.
                    logger.LogError(ex, "Attachment sweep failed");
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
