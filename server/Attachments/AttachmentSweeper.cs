namespace Vorcall.Server.Attachments;

// Uploads nothing ever linked are the one kind of attachment no other path deletes: a client
// that picks a file and then abandons the composer leaves a row and a file behind. Deleting a
// message takes its own attachments with it, so this only ever sees the unlinked ones. Images are
// the same story with a different reference — a profile, the server row or a role — and are swept
// on the same schedule. A soundpad clip is referenced by nothing and is only ever deleted on
// purpose, so its two passes reclaim what an upload that died mid-flight left behind: the rows
// whose bytes never arrived, and the files no row names. An attachment row whose bytes never
// finished arriving waits for the far longer incomplete cutoff instead, since a large upload can
// still be in flight — a 16 MiB clip cannot, which is why its own cutoff is the short one.
public sealed class AttachmentSweeper(
    AttachmentStore store,
    ImageStore images,
    SoundStore sounds,
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
                    var removed = await store.SweepUnlinkedAsync(
                        now - AttachmentsOptions.UnlinkedTtl,
                        now - AttachmentsOptions.IncompleteTtl);
                    var removedImages = await images.SweepUnreferencedAsync(
                        AttachmentsOptions.UnlinkedTtl,
                        now,
                        stoppingToken);
                    var incompleteSounds = await sounds.SweepIncompleteAsync(
                        now - AttachmentsOptions.UnlinkedTtl,
                        stoppingToken);
                    var orphanSoundFiles = await sounds.SweepOrphanFilesAsync(stoppingToken);
                    if (removed > 0 || removedImages > 0 || incompleteSounds > 0 || orphanSoundFiles > 0)
                    {
                        logger.LogInformation(
                            "Swept {Count} unlinked attachments, {ImageCount} unreferenced images, "
                            + "{SoundCount} incomplete sounds and {SoundFileCount} orphaned sound files",
                            removed,
                            removedImages,
                            incompleteSounds,
                            orphanSoundFiles);
                    }
                    else
                    {
                        logger.LogDebug(
                            "Swept no unlinked attachments, no unreferenced images, no incomplete sounds and no orphaned sound files");
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
