using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Chat;
using Vorcall.Server.Data;
using Vorcall.Server.Metrics;

namespace Vorcall.Server.Attachments;

public sealed record StickerRecord(long Id, string Name, long? UploaderId, string ContentType, long Size);

// Sticker is set only when Status is Saved.
public sealed record StickerSaveOutcome(StickerSaveOutcome.Kind Status, StickerRecord? Sticker)
{
    public enum Kind
    {
        Saved,
        TooLarge,
        BadMagic,

        // The name is not one the 32-scalar grammar accepts.
        InvalidName,
        Truncated,
        QuotaExceeded,

        // The library already holds AttachmentsOptions.MaxStickers complete stickers.
        LimitReached,

        // The upload was fine and this server could not write it down.
        IoError,
    }

    public static StickerSaveOutcome Rejected(Kind kind) => new(kind, null);

    public static StickerSaveOutcome Saved(StickerRecord sticker) => new(Kind.Saved, sticker);
}

// The server-wide sticker library: the soundpad's store with an image's body rules — the same
// streaming upload and storage quota, in a store of its own under stickers/, holding one of the
// four image types, magic-checked as it streams, at 1 MiB.
public sealed class StickerStore(
    AttachmentsOptions options,
    IDbContextFactory<AppDbContext> contexts,
    ServerMetrics metrics,
    ILogger<StickerStore> logger)
{
    public const string SubDirectory = "stickers";

    // Every sticker is stored under its row id and this one extension: the type is a column and
    // the name a member gave it is metadata, neither ever part of a path.
    public const string StoredExtension = "bin";

    private const int CopyBufferBytes = 64 * 1024;
    private const string PartSuffix = ".part";

    private string Root => Path.Combine(Path.TrimEndingDirectorySeparator(Path.GetFullPath(options.Dir)), SubDirectory);

    public string PathFor(long id)
    {
        var root = Root;
        var path = Path.GetFullPath(Path.Combine(root, $"{id}.{StoredExtension}"));

        // Ids are numbers and the extension is a literal, so there is no traversal to filter out;
        // this is the assertion that keeps it that way.
        if (Path.GetDirectoryName(path) != root)
        {
            throw new InvalidOperationException("Sticker path escaped the stickers directory.");
        }

        return path;
    }

    // The quota is the attachments' (Vorcall:AttachmentsMaxBytes) and shared with them, so all
    // four tables count here, in ImageStore, AttachmentStore and SoundStore alike.
    public async Task<long> TotalBytesAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var stickers = await db.Stickers.SumAsync(s => (long?)s.Size, ct) ?? 0;
        var sounds = await db.Sounds.SumAsync(s => (long?)s.Size, ct) ?? 0;
        var images = await db.Images.SumAsync(i => (long?)i.Size, ct) ?? 0;
        var attachments = await db.Attachments.SumAsync(a => (long?)a.Size, ct) ?? 0;
        return stickers + sounds + images + attachments;
    }

    public async Task<StickerSaveOutcome> SaveAsync(
        string name,
        long uploaderId,
        string contentType,
        long declaredLength,
        Stream body,
        CancellationToken ct)
    {
        if (!Names.TryNormalize(name, out var normalized))
        {
            return StickerSaveOutcome.Rejected(StickerSaveOutcome.Kind.InvalidName);
        }

        if (!AttachmentsOptions.TryImageExtension(contentType, out _))
        {
            return StickerSaveOutcome.Rejected(StickerSaveOutcome.Kind.BadMagic);
        }

        if (declaredLength > AttachmentsOptions.StickerMaxFileBytes)
        {
            return StickerSaveOutcome.Rejected(StickerSaveOutcome.Kind.TooLarge);
        }

        await using var db = await contexts.CreateDbContextAsync(ct);

        if (await db.Stickers.CountAsync(s => s.Complete, ct) >= AttachmentsOptions.MaxStickers)
        {
            return StickerSaveOutcome.Rejected(StickerSaveOutcome.Kind.LimitReached);
        }

        var storedBytes = await TotalBytesAsync(ct);
        if (storedBytes + declaredLength > options.MaxBytes)
        {
            logger.LogWarning(
                "Sticker upload by {UserId} refused: {StoredBytes} stored plus {DeclaredBytes} declared exceeds the {QuotaBytes} quota",
                uploaderId,
                storedBytes,
                declaredLength,
                options.MaxBytes);
            return StickerSaveOutcome.Rejected(StickerSaveOutcome.Kind.QuotaExceeded);
        }

        // The row is written first because the id names the file, and stays incomplete until the
        // bytes are under that name.
        var row = new Sticker
        {
            Name = normalized,
            UploaderId = uploaderId,
            ContentType = contentType,
            Size = declaredLength,
            Complete = false,
            CreatedAt = DateTime.UtcNow,
        };

        db.Stickers.Add(row);
        await db.SaveChangesAsync(ct);

        var path = PathFor(row.Id);
        var partPath = path + PartSuffix;
        StickerSaveOutcome.Kind? failure = null;
        try
        {
            Directory.CreateDirectory(Root);

            var header = new byte[AttachmentsOptions.ImageMagicLength];
            var headerLength = 0;
            var written = 0L;

            // Written to a .part name and renamed at the end, so a file under the final name is
            // always a complete upload of the declared length.
            await using (var file = new FileStream(partPath, FileMode.Create, FileAccess.Write, FileShare.None))
            {
                var buffer = new byte[CopyBufferBytes];
                while (true)
                {
                    int read;
                    try
                    {
                        read = await body.ReadAsync(buffer, ct);
                    }
                    catch (IOException)
                    {
                        // Kestrel reports a client that dropped mid-body as an IOException: a
                        // truncated upload, not this server failing to write it down.
                        logger.LogDebug("Sticker {StickerId} upload aborted", row.Id);
                        failure = StickerSaveOutcome.Kind.Truncated;
                        break;
                    }

                    if (read == 0)
                    {
                        break;
                    }

                    written += read;
                    if (written > declaredLength)
                    {
                        failure = StickerSaveOutcome.Kind.TooLarge;
                        break;
                    }

                    if (headerLength < header.Length)
                    {
                        var copied = Math.Min(header.Length - headerLength, read);
                        buffer.AsSpan(0, copied).CopyTo(header.AsSpan(headerLength));
                        headerLength += copied;
                        if (headerLength == header.Length && !AttachmentsOptions.MatchesImageMagic(contentType, header))
                        {
                            failure = StickerSaveOutcome.Kind.BadMagic;
                            break;
                        }
                    }

                    await file.WriteAsync(buffer.AsMemory(0, read), ct);
                }
            }

            // A body too short to carry a magic number is not one of the four types either.
            if (failure is null && headerLength < header.Length)
            {
                failure = StickerSaveOutcome.Kind.BadMagic;
            }

            if (failure is null && written != declaredLength)
            {
                failure = StickerSaveOutcome.Kind.Truncated;
            }

            if (failure is null)
            {
                File.Move(partPath, path, overwrite: true);
            }
        }
        catch (OperationCanceledException)
        {
            logger.LogDebug("Sticker {StickerId} upload aborted", row.Id);
            failure = StickerSaveOutcome.Kind.Truncated;
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // The caller sent a perfectly good image and the disk refused it, so this is not a
            // rejected upload: it is this server failing, and the only line that says so.
            logger.LogError(ex, "Sticker {StickerId} could not be written to storage", row.Id);
            failure = StickerSaveOutcome.Kind.IoError;
        }

        if (failure is { } rejected)
        {
            TryDeleteFile(partPath);

            // Not ct: a cancelled upload still has to take its row with it.
            db.Stickers.Remove(row);
            await db.SaveChangesAsync(CancellationToken.None);
            return StickerSaveOutcome.Rejected(rejected);
        }

        // Only after the rename, so the flag is true exactly when the bytes are on disk under the
        // final name. Not ct, for the same reason the removal above is not.
        row.Complete = true;
        await db.SaveChangesAsync(CancellationToken.None);

        logger.LogDebug("Sticker {StickerId} stored for {UserId} ({SizeBytes} bytes)", row.Id, uploaderId, row.Size);
        metrics.CountStickerUpload();
        return StickerSaveOutcome.Saved(ToRecord(row));
    }

    // The row and the file it names, for the download endpoint to stream; null when the id names
    // no sticker. An incomplete row is not one: its bytes are not under that name yet.
    public async Task<(StickerRecord Sticker, string Path)?> GetAsync(long id, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var row = await db.Stickers.AsNoTracking().FirstOrDefaultAsync(s => s.Id == id && s.Complete, ct);
        return row is null ? null : (ToRecord(row), PathFor(row.Id));
    }

    // The whole library: it is server-wide and every member sees all of it. Incomplete rows are
    // uploads still in flight and are no part of it.
    public async Task<List<StickerRecord>> ListAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var rows = await db.Stickers.AsNoTracking().Where(s => s.Complete).OrderBy(s => s.Id).ToListAsync(ct);
        return rows.ConvertAll(ToRecord);
    }

    public async Task<bool> RenameAsync(long id, string name, CancellationToken ct)
    {
        if (!Names.TryNormalize(name, out var normalized))
        {
            return false;
        }

        await using var db = await contexts.CreateDbContextAsync(ct);
        var row = await db.Stickers.FirstOrDefaultAsync(s => s.Id == id, ct);
        if (row is null)
        {
            return false;
        }

        row.Name = normalized;
        await db.SaveChangesAsync(ct);
        return true;
    }

    // Row and file together. The messages that sent it keep their place: the foreign key nulls
    // their sticker_id and leaves them flagged as sticker messages.
    public async Task<bool> DeleteAsync(long id, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var deleted = await db.Stickers.Where(s => s.Id == id).ExecuteDeleteAsync(ct);
        if (deleted == 0)
        {
            return false;
        }

        DeleteFiles(id);
        return true;
    }

    // An upload whose bytes never finished arriving: the process died between the row and the
    // rename, and nothing else will ever complete it. The short unlinked cutoff, like a sound's:
    // a 1 MiB body cannot take an hour to arrive.
    public async Task<int> SweepIncompleteAsync(DateTime olderThan, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var stale = await db.Stickers
            .AsNoTracking()
            .Where(s => !s.Complete && s.CreatedAt < olderThan)
            .Select(s => s.Id)
            .ToListAsync(ct);
        if (stale.Count == 0)
        {
            return 0;
        }

        var deleted = await db.Stickers
            .Where(s => stale.Contains(s.Id) && !s.Complete)
            .ExecuteDeleteAsync(ct);

        // Anything still there completed between the read and the delete, and its file is now the
        // library's.
        var survivors = await db.Stickers
            .AsNoTracking()
            .Where(s => stale.Contains(s.Id))
            .Select(s => s.Id)
            .ToListAsync(ct);

        foreach (var id in stale.Where(id => !survivors.Contains(id)))
        {
            DeleteFiles(id);
        }

        logger.LogDebug("Swept {Count} incomplete stickers older than {Cutoff}", deleted, olderThan);
        return deleted;
    }

    // Files an upload left behind when the process died between writing them and writing (or
    // clearing) their row: the id has no row at all, or the final file is already there and the
    // .part beside it is the leftover of a retry. An in-flight upload is neither, since its row
    // exists and its final name does not.
    public async Task<int> SweepOrphanFilesAsync(CancellationToken ct)
    {
        List<string> files;
        try
        {
            files = Directory.EnumerateFiles(Root).ToList();
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or ArgumentException)
        {
            logger.LogDebug("Could not list sticker files ({Reason})", ex.GetType().Name);
            return 0;
        }

        var byId = new Dictionary<long, List<string>>();
        foreach (var file in files)
        {
            var stem = Path.GetFileName(file);
            var dot = stem.IndexOf('.');
            if (dot <= 0 || !long.TryParse(stem.AsSpan(0, dot), out var id))
            {
                continue;
            }

            if (!byId.TryGetValue(id, out var group))
            {
                group = [];
                byId[id] = group;
            }

            group.Add(file);
        }

        if (byId.Count == 0)
        {
            return 0;
        }

        var ids = byId.Keys.ToList();
        await using var db = await contexts.CreateDbContextAsync(ct);
        var known = await db.Stickers
            .AsNoTracking()
            .Where(s => ids.Contains(s.Id))
            .Select(s => s.Id)
            .ToListAsync(ct);

        var deleted = 0;
        foreach (var (id, group) in byId)
        {
            var orphan = !known.Contains(id);
            foreach (var file in group)
            {
                var stale = orphan
                    || (file.EndsWith(PartSuffix, StringComparison.Ordinal) && File.Exists(PathFor(id)));
                if (!stale)
                {
                    continue;
                }

                TryDeleteFile(file);
                deleted++;
            }
        }

        if (deleted > 0)
        {
            logger.LogDebug("Swept {Count} orphaned sticker files", deleted);
        }

        return deleted;
    }

    private static StickerRecord ToRecord(Sticker row)
        => new(row.Id, row.Name, row.UploaderId, row.ContentType, row.Size);

    // Best effort by design: the row is gone, and a file left behind is disk to reclaim, not a
    // correctness problem. Globs the id rather than rebuilding a name, so an abandoned .part of
    // the same id goes with it.
    private void DeleteFiles(long id)
    {
        IEnumerable<string> files;
        try
        {
            files = Directory.EnumerateFiles(Root, $"{id}.*").ToList();
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or ArgumentException)
        {
            logger.LogDebug("Could not list sticker files for {StickerId} ({Reason})", id, ex.GetType().Name);
            return;
        }

        foreach (var path in files)
        {
            TryDeleteFile(path);
        }
    }

    private void TryDeleteFile(string path)
    {
        try
        {
            File.Delete(path);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or NotSupportedException or ArgumentException)
        {
            logger.LogDebug("Could not delete sticker file {Path} ({Reason})", path, ex.GetType().Name);
        }
    }
}
