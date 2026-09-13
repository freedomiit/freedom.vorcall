using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Data;

namespace Vorcall.Server.Streams;

// The streamed_files rows: the whole of what the server keeps of a streamed file, since the bytes
// stay on the owner's disk. The link to a message and the deletes that go with a message's
// tombstone run inside MessageService's and MemberDirectory's own transactions, beside the
// attachments' — what lives here is what needs no other row.
public sealed class StreamDirectory(IDbContextFactory<AppDbContext> contexts, ILogger<StreamDirectory> logger)
{
    public async Task<Protocol.StreamedFile> OfferAsync(
        long ownerId,
        long channelId,
        string fileName,
        string contentType,
        long size,
        CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var row = new StreamedFile
        {
            ChannelId = channelId,
            OwnerId = ownerId,
            FileName = fileName,
            ContentType = contentType,
            Size = size,
            CreatedAt = DateTime.UtcNow,
        };
        db.StreamedFiles.Add(row);
        await db.SaveChangesAsync(ct);

        logger.LogDebug(
            "Streamed file {StreamId} offered by {UserId} to {ChannelId} ({Size} bytes)",
            row.Id,
            ownerId,
            channelId,
            size);
        return ToProtocol(row);
    }

    public async Task<StreamedFile?> FindAsync(long id, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        return await db.StreamedFiles.AsNoTracking().FirstOrDefaultAsync(s => s.Id == id, ct);
    }

    // Offers nothing ever linked. There is no file to reclaim, but a row that names an owner and a
    // channel for good is still a row nothing will ever read.
    public async Task<int> SweepUnlinkedAsync(DateTime olderThan, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var deleted = await db.StreamedFiles
            .Where(s => s.MessageId == null && s.CreatedAt < olderThan)
            .ExecuteDeleteAsync(ct);
        if (deleted > 0)
        {
            logger.LogDebug("Swept {Count} unlinked streamed files older than {Cutoff}", deleted, olderThan);
        }

        return deleted;
    }

    // 0 for an owner whose account is gone, as PROTOCOL.md has every absent id.
    public static Protocol.StreamedFile ToProtocol(StreamedFile row) => new()
    {
        Id = row.Id,
        FileName = row.FileName,
        ContentType = row.ContentType,
        Size = row.Size,
        OwnerId = row.OwnerId ?? 0,
    };
}
