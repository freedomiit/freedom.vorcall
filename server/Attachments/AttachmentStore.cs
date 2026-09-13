using System.Text;
using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Data;
using Vorcall.Server.Metrics;

namespace Vorcall.Server.Attachments;

// Attachment is set only when Status is Stored.
public sealed record StoreOutcome(StoreOutcome.Kind Status, Protocol.Attachment? Attachment)
{
    public enum Kind
    {
        Stored,
        TooLarge,
        QuotaExceeded,

        // The declared media type is not one this server can record: any type is accepted, a
        // header that is not a type/subtype at all is not.
        BadContentType,
        Truncated,

        // The upload was fine and this server could not write it down.
        StorageFailed,
    }

    public static StoreOutcome Rejected(Kind kind) => new(kind, null);

    public static StoreOutcome Stored(Protocol.Attachment attachment) => new(Kind.Stored, attachment);
}

// The row and the file, always together: nothing here leaves a row without its bytes or bytes
// without their row. The file is named after the row id, so the name a client sent is metadata
// and never a path.
public sealed class AttachmentStore(
    AttachmentsOptions options,
    IDbContextFactory<AppDbContext> contexts,
    ServerMetrics metrics,
    ILogger<AttachmentStore> logger)
{
    // Matches attachments.file_name, varchar(255).
    public const int MaxFileNameLength = 255;

    private const int CopyBufferBytes = 64 * 1024;
    private const string PartSuffix = ".part";

    // What an upload that named no file is called; an attachment is any file, so nothing here
    // pretends it is a picture.
    private const string FallbackFileName = "file.bin";

    // <dir>/<id>.bin, where everything written from now on goes. The stored name depends on the
    // row id alone and not on the type the client declared, which is exactly what lets an
    // attachment be any file at all.
    public string PathFor(long id) => PathWithExtension(id, AttachmentsOptions.StoredExtension);

    // The path a row's bytes are actually under. Until 0.6.0 an attachment was stored as
    // <id>.<ext> with the extension derived from its content type, so a row written before then
    // still points at that name and neither a download nor a delete may miss it. Falls back to the
    // current name when neither file is there, so a caller's "no file on disk" branch still fires.
    public string ExistingPathFor(long id, string contentType)
    {
        var path = PathFor(id);
        if (File.Exists(path))
        {
            return path;
        }

        if (AttachmentsOptions.TryImageExtension(contentType, out var ext))
        {
            var legacy = PathWithExtension(id, ext);
            if (File.Exists(legacy))
            {
                return legacy;
            }
        }

        return path;
    }

    // Kept for callers that hold a whole row rather than an id; they get the resolved name, since
    // a row is exactly what says which of the two a file could be under.
    public string PathFor(long id, string contentType) => ExistingPathFor(id, contentType);

    // Images and soundpad clips share this quota (Vorcall:AttachmentsMaxBytes), so all three
    // tables count towards it.
    public async Task<long> TotalBytesAsync()
    {
        await using var db = await contexts.CreateDbContextAsync();
        var attachments = await db.Attachments.SumAsync(a => (long?)a.Size) ?? 0;
        var images = await db.Images.SumAsync(i => (long?)i.Size) ?? 0;
        var sounds = await db.Sounds.SumAsync(s => (long?)s.Size) ?? 0;
        return attachments + images + sounds;
    }

    public async Task<StoreOutcome> StoreAsync(
        long uploaderId,
        long channelId,
        string contentType,
        string? fileName,
        long declaredLength,
        Stream body,
        CancellationToken ct)
    {
        // Any type, as long as it is a type: the bytes are never interpreted, so the only thing
        // that could be wrong with the declaration is its shape.
        if (!AttachmentsOptions.IsValidMediaType(contentType))
        {
            return StoreOutcome.Rejected(StoreOutcome.Kind.BadContentType);
        }

        if (declaredLength > AttachmentsOptions.MaxFileBytes)
        {
            return StoreOutcome.Rejected(StoreOutcome.Kind.TooLarge);
        }

        var storedBytes = await TotalBytesAsync();
        if (storedBytes + declaredLength > options.MaxBytes)
        {
            logger.LogWarning(
                "Attachment upload by {UserId} refused: {StoredBytes} stored plus {DeclaredBytes} declared exceeds the {QuotaBytes} quota",
                uploaderId,
                storedBytes,
                declaredLength,
                options.MaxBytes);
            return StoreOutcome.Rejected(StoreOutcome.Kind.QuotaExceeded);
        }

        await using var db = await contexts.CreateDbContextAsync();

        // The row is written first because the id names the file, and stays incomplete until the
        // bytes are under that name.
        var row = new Data.Attachment
        {
            ChannelId = channelId,
            UploaderId = uploaderId,
            MessageId = null,
            FileName = SanitizeFileName(fileName),
            ContentType = contentType,
            Size = declaredLength,
            Complete = false,
            CreatedAt = DateTime.UtcNow,
        };

        db.Attachments.Add(row);
        await db.SaveChangesAsync(ct);

        var path = PathFor(row.Id);
        var partPath = path + PartSuffix;
        StoreOutcome.Kind? failure = null;
        try
        {
            Directory.CreateDirectory(options.Dir);

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
                        // Kestrel reports a client that dropped mid-body as an IOException
                        // (BadHttpRequestException, ConnectionResetException): a truncated upload,
                        // not this server failing to write it down.
                        logger.LogDebug("Attachment {AttachmentId} upload aborted", row.Id);
                        failure = StoreOutcome.Kind.Truncated;
                        break;
                    }

                    if (read == 0)
                    {
                        break;
                    }

                    written += read;
                    if (written > declaredLength)
                    {
                        failure = StoreOutcome.Kind.TooLarge;
                        break;
                    }

                    await file.WriteAsync(buffer.AsMemory(0, read), ct);
                }
            }

            if (failure is null && written != declaredLength)
            {
                failure = StoreOutcome.Kind.Truncated;
            }

            if (failure is null)
            {
                File.Move(partPath, path, overwrite: true);
            }
        }
        catch (OperationCanceledException)
        {
            logger.LogDebug("Attachment {AttachmentId} upload aborted", row.Id);
            failure = StoreOutcome.Kind.Truncated;
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // The caller sent a perfectly good file and the disk refused it, so this is not a
            // rejected upload: it is this server failing, and the only line that says so.
            logger.LogError(ex, "Attachment {AttachmentId} could not be written to storage", row.Id);
            failure = StoreOutcome.Kind.StorageFailed;
        }

        if (failure is { } rejected)
        {
            TryDeleteFile(partPath);

            // Not ct: a cancelled upload still has to take its row with it.
            db.Attachments.Remove(row);
            await db.SaveChangesAsync(CancellationToken.None);
            return StoreOutcome.Rejected(rejected);
        }

        // Only after the rename, so the flag is true exactly when the bytes are on disk under the
        // final name. Not ct, for the same reason the removal above is not.
        row.Complete = true;
        await db.SaveChangesAsync(CancellationToken.None);

        logger.LogDebug(
            "Attachment {AttachmentId} stored for {UserId} in {ChannelId} ({SizeBytes} bytes)",
            row.Id,
            uploaderId,
            channelId,
            row.Size);
        metrics.CountUpload();
        return StoreOutcome.Stored(new Protocol.Attachment
        {
            Id = row.Id,
            FileName = row.FileName,
            ContentType = row.ContentType,
            Size = row.Size,
        });
    }

    // The entity rather than the protocol message: the download endpoint decides what a caller
    // may see from MessageId, UploaderId and ChannelId.
    public async Task<Data.Attachment?> FindAsync(long id)
    {
        await using var db = await contexts.CreateDbContextAsync();
        return await db.Attachments.AsNoTracking().FirstOrDefaultAsync(a => a.Id == id);
    }

    // Best effort by design: the rows are already gone, and a file left behind is disk to
    // reclaim, not a correctness problem. The .part sibling goes too, because this is the overload
    // the sweeper uses and an incomplete row's bytes are under exactly that name — once its row is
    // gone nothing is left to find them by.
    public void DeleteFiles(IEnumerable<(long Id, string ContentType)> rows)
    {
        foreach (var path in PathsFor(rows))
        {
            TryDeleteFile(path);
            TryDeleteFile(path + PartSuffix);
        }
    }

    public void DeleteFiles(IEnumerable<string> paths)
    {
        foreach (var path in paths)
        {
            TryDeleteFile(path);
        }
    }

    // The paths of rows whose database entries are about to go, or have just gone through a
    // cascade: a caller that can no longer read a content type keeps the names instead. Resolved
    // rather than built, because a row written before 0.6.0 is under the old name and deleting its
    // row while leaving the file would leak disk with nothing left to find the orphan by.
    public List<string> PathsFor(IEnumerable<(long Id, string ContentType)> rows)
    {
        var paths = new List<string>();
        foreach (var (id, contentType) in rows)
        {
            paths.Add(ExistingPathFor(id, contentType));
        }

        return paths;
    }

    // Two cutoffs, because the row exists before its bytes do. A complete upload nothing linked is
    // stale on the caller's short cutoff; an incomplete one may still be streaming — 2 GiB on a
    // slow link outlives that cutoff easily — so it is only swept once it is past the longer one.
    public async Task<int> SweepUnlinkedAsync(DateTime olderThan, DateTime incompleteOlderThan)
    {
        await using var db = await contexts.CreateDbContextAsync();
        var stale = await db.Attachments
            .AsNoTracking()
            .Where(a => a.MessageId == null
                && ((a.Complete && a.CreatedAt < olderThan) || (!a.Complete && a.CreatedAt < incompleteOlderThan)))
            .Select(a => new { a.Id, a.ContentType })
            .ToListAsync();
        if (stale.Count == 0)
        {
            return 0;
        }

        var ids = stale.Select(a => a.Id).ToList();
        var deleted = await db.Attachments
            .Where(a => ids.Contains(a.Id) && a.MessageId == null)
            .ExecuteDeleteAsync();

        // Anything still there was linked to a message between the read and the delete, and its
        // file now belongs to that message.
        var survivors = await db.Attachments
            .AsNoTracking()
            .Where(a => ids.Contains(a.Id))
            .Select(a => a.Id)
            .ToListAsync();

        DeleteFiles(stale.Where(a => !survivors.Contains(a.Id)).Select(a => (a.Id, a.ContentType)));
        logger.LogDebug(
            "Swept {Count} unlinked attachments: complete ones older than {Cutoff}, incomplete ones older than {IncompleteCutoff}",
            deleted,
            olderThan,
            incompleteOlderThan);
        return deleted;
    }

    // Metadata only: the name is echoed to clients and never used to build a path.
    public static string SanitizeFileName(string? raw)
    {
        if (string.IsNullOrWhiteSpace(raw))
        {
            return FallbackFileName;
        }

        var builder = new StringBuilder(MaxFileNameLength);
        foreach (var rune in Path.GetFileName(raw.Trim()).EnumerateRunes())
        {
            // GetFileName only strips the platform's own separators, so a Windows-style path in
            // the header still arrives with backslashes in it.
            if (Rune.IsControl(rune) || rune.Value is '/' or '\\')
            {
                continue;
            }

            if (builder.Length + rune.Utf16SequenceLength > MaxFileNameLength)
            {
                break;
            }

            builder.Append(rune);
        }

        var name = builder.ToString().Trim();
        return name.Length == 0 ? FallbackFileName : name;
    }

    private string PathWithExtension(long id, string ext)
    {
        var root = Path.TrimEndingDirectorySeparator(Path.GetFullPath(options.Dir));
        var path = Path.GetFullPath(Path.Combine(root, $"{id}.{ext}"));

        // Ids are numbers and the extension is one of five literals — the stored one, or one of
        // the four a pre-0.6.0 row could have been written under — so there is no traversal to
        // filter out; this is the assertion that keeps it that way.
        if (Path.GetDirectoryName(path) != root)
        {
            throw new InvalidOperationException("Attachment path escaped the attachments directory.");
        }

        return path;
    }

    private void TryDeleteFile(string path)
    {
        try
        {
            File.Delete(path);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or NotSupportedException or ArgumentException)
        {
            logger.LogDebug("Could not delete attachment file {Path} ({Reason})", path, ex.GetType().Name);
        }
    }
}
