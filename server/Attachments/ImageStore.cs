using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Data;
using Vorcall.Server.Metrics;

namespace Vorcall.Server.Attachments;

public sealed record ImageRecord(long Id, ImagePurpose Purpose, long? UploaderId, string ContentType, long Size);

// Image is set only when Status is Saved.
public sealed record ImageSaveOutcome(ImageSaveOutcome.Kind Status, ImageRecord? Image)
{
    public enum Kind
    {
        Saved,
        TooLarge,
        BadMagic,
        Truncated,
        QuotaExceeded,

        // The upload was fine and this server could not write it down.
        IoError,
    }

    public static ImageSaveOutcome Rejected(Kind kind) => new(kind, null);

    public static ImageSaveOutcome Saved(ImageRecord image) => new(Kind.Saved, image);
}

// Avatars, banners, the server icon and role icons: the same streaming upload and the same
// storage quota as attachments, in a store of their own under images/, but held to rules an
// attachment is not — four types, each magic-checked, at 8 MiB. An image is kept while a profile,
// the server row or a role points at it and swept with its file once nothing does.
public sealed class ImageStore(
    AttachmentsOptions options,
    IDbContextFactory<AppDbContext> contexts,
    ServerMetrics metrics,
    ILogger<ImageStore> logger)
{
    public const string SubDirectory = "images";

    private const int CopyBufferBytes = 64 * 1024;
    private const string PartSuffix = ".part";

    private string Root => Path.Combine(Path.TrimEndingDirectorySeparator(Path.GetFullPath(options.Dir)), SubDirectory);

    public string PathFor(long id, string contentType)
    {
        if (!AttachmentsOptions.TryImageExtension(contentType, out var ext))
        {
            throw new ArgumentException($"Unsupported image content type '{contentType}'.", nameof(contentType));
        }

        var root = Root;
        var path = Path.GetFullPath(Path.Combine(root, $"{id}.{ext}"));

        // Ids are numbers and the extension is one of four literals, so there is no traversal to
        // filter out; this is the assertion that keeps it that way.
        if (Path.GetDirectoryName(path) != root)
        {
            throw new InvalidOperationException("Image path escaped the images directory.");
        }

        return path;
    }

    // The quota is the attachments' (Vorcall:AttachmentsMaxBytes) and shared with them and with
    // the soundpad, so all three tables count here, in AttachmentStore and in SoundStore alike.
    public async Task<long> TotalBytesAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var images = await db.Images.SumAsync(i => (long?)i.Size, ct) ?? 0;
        var attachments = await db.Attachments.SumAsync(a => (long?)a.Size, ct) ?? 0;
        var sounds = await db.Sounds.SumAsync(s => (long?)s.Size, ct) ?? 0;
        return images + attachments + sounds;
    }

    public async Task<ImageSaveOutcome> SaveAsync(
        ImagePurpose purpose,
        long uploaderId,
        string contentType,
        long declaredLength,
        Stream body,
        CancellationToken ct)
    {
        if (!AttachmentsOptions.TryImageExtension(contentType, out _))
        {
            return ImageSaveOutcome.Rejected(ImageSaveOutcome.Kind.BadMagic);
        }

        if (declaredLength > AttachmentsOptions.ImageMaxFileBytes)
        {
            return ImageSaveOutcome.Rejected(ImageSaveOutcome.Kind.TooLarge);
        }

        var storedBytes = await TotalBytesAsync(ct);
        if (storedBytes + declaredLength > options.MaxBytes)
        {
            logger.LogWarning(
                "Image upload by {UserId} refused: {StoredBytes} stored plus {DeclaredBytes} declared exceeds the {QuotaBytes} quota",
                uploaderId,
                storedBytes,
                declaredLength,
                options.MaxBytes);
            return ImageSaveOutcome.Rejected(ImageSaveOutcome.Kind.QuotaExceeded);
        }

        await using var db = await contexts.CreateDbContextAsync(ct);

        // The row is written first because the id names the file.
        var row = new Data.Image
        {
            Purpose = purpose,
            UploaderId = uploaderId,
            ContentType = contentType,
            Size = declaredLength,
            CreatedAt = DateTime.UtcNow,
        };

        db.Images.Add(row);
        await db.SaveChangesAsync(ct);

        var path = PathFor(row.Id, contentType);
        var partPath = path + PartSuffix;
        ImageSaveOutcome.Kind? failure = null;
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
                        logger.LogDebug("Image {ImageId} upload aborted", row.Id);
                        failure = ImageSaveOutcome.Kind.Truncated;
                        break;
                    }

                    if (read == 0)
                    {
                        break;
                    }

                    written += read;
                    if (written > declaredLength)
                    {
                        failure = ImageSaveOutcome.Kind.TooLarge;
                        break;
                    }

                    if (headerLength < header.Length)
                    {
                        var copied = Math.Min(header.Length - headerLength, read);
                        buffer.AsSpan(0, copied).CopyTo(header.AsSpan(headerLength));
                        headerLength += copied;
                        if (headerLength == header.Length && !AttachmentsOptions.MatchesImageMagic(contentType, header))
                        {
                            failure = ImageSaveOutcome.Kind.BadMagic;
                            break;
                        }
                    }

                    await file.WriteAsync(buffer.AsMemory(0, read), ct);
                }
            }

            // A body too short to carry a magic number is not one of the four types either.
            if (failure is null && headerLength < header.Length)
            {
                failure = ImageSaveOutcome.Kind.BadMagic;
            }

            if (failure is null && written != declaredLength)
            {
                failure = ImageSaveOutcome.Kind.Truncated;
            }

            if (failure is null)
            {
                File.Move(partPath, path, overwrite: true);
            }
        }
        catch (OperationCanceledException)
        {
            logger.LogDebug("Image {ImageId} upload aborted", row.Id);
            failure = ImageSaveOutcome.Kind.Truncated;
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // The caller sent a perfectly good image and the disk refused it, so this is not a
            // rejected upload: it is this server failing, and the only line that says so.
            logger.LogError(ex, "Image {ImageId} could not be written to storage", row.Id);
            failure = ImageSaveOutcome.Kind.IoError;
        }

        if (failure is { } rejected)
        {
            TryDeleteFile(partPath);

            // Not ct: a cancelled upload still has to take its row with it.
            db.Images.Remove(row);
            await db.SaveChangesAsync(CancellationToken.None);
            return ImageSaveOutcome.Rejected(rejected);
        }

        logger.LogDebug(
            "Image {ImageId} stored for {UserId} as {Purpose} ({SizeBytes} bytes)",
            row.Id,
            uploaderId,
            purpose,
            row.Size);
        metrics.CountImageUpload();
        return ImageSaveOutcome.Saved(ToRecord(row));
    }

    // The row and the file it names, for the download endpoint to stream; null when the id names
    // no image.
    public async Task<(ImageRecord Image, string Path)?> GetAsync(long id, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var row = await db.Images.AsNoTracking().FirstOrDefaultAsync(i => i.Id == id, ct);
        if (row is null || !AttachmentsOptions.TryImageExtension(row.ContentType, out _))
        {
            return null;
        }

        return (ToRecord(row), PathFor(row.Id, row.ContentType));
    }

    // Whether an image id may be referenced by the frame that named it: it exists, carries the
    // expected purpose and — when the caller must own it, as for an avatar or a banner — was
    // uploaded by that account.
    public async Task<bool> ResolveForAsync(long id, ImagePurpose expected, long? mustBeUploadedBy, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var query = db.Images.Where(i => i.Id == id && i.Purpose == expected);
        if (mustBeUploadedBy is { } uploader)
        {
            query = query.Where(i => i.UploaderId == uploader);
        }

        return await query.AnyAsync(ct);
    }

    // Best effort by design: the rows are gone (or going), and a file left behind is disk to
    // reclaim, not a correctness problem. The extension comes off the directory because the
    // content type is no longer readable once the row has been deleted.
    public void DeleteFiles(IEnumerable<long> ids)
    {
        foreach (var id in ids)
        {
            foreach (var path in EnumerateFiles(id))
            {
                TryDeleteFile(path);
            }
        }
    }

    public async Task<int> DeleteAsync(IEnumerable<long> ids, CancellationToken ct)
    {
        var wanted = ids.Distinct().ToList();
        if (wanted.Count == 0)
        {
            return 0;
        }

        await using var db = await contexts.CreateDbContextAsync(ct);
        var deleted = await db.Images.Where(i => wanted.Contains(i.Id)).ExecuteDeleteAsync(ct);
        DeleteFiles(wanted);
        return deleted;
    }

    // An image nothing points at is the one kind no other path deletes: a picker abandoned before
    // the frame that would have referenced it, or a reference that has since been replaced.
    public async Task<int> SweepUnreferencedAsync(TimeSpan olderThan, DateTime now, CancellationToken ct)
    {
        var cutoff = now - olderThan;
        await using var db = await contexts.CreateDbContextAsync(ct);

        var stale = await Unreferenced(db)
            .AsNoTracking()
            .Where(i => i.CreatedAt < cutoff)
            .Select(i => i.Id)
            .ToListAsync(ct);
        if (stale.Count == 0)
        {
            return 0;
        }

        // The delete re-evaluates the reference check, so an image something started pointing at
        // between the read and the delete keeps its row.
        var deleted = await Unreferenced(db)
            .Where(i => stale.Contains(i.Id) && i.CreatedAt < cutoff)
            .ExecuteDeleteAsync(ct);

        var survivors = await db.Images
            .AsNoTracking()
            .Where(i => stale.Contains(i.Id))
            .Select(i => i.Id)
            .ToListAsync(ct);

        DeleteFiles(stale.Where(id => !survivors.Contains(id)));
        logger.LogDebug("Swept {Count} unreferenced images older than {Cutoff}", deleted, cutoff);
        return deleted;
    }

    // Takes the caller's context so the answer is read inside the caller's own transaction: a
    // reference it has just written, or just cleared, is part of the question.
    public static async Task<bool> IsReferencedAsync(AppDbContext db, long imageId, CancellationToken ct)
        => await db.Users.AnyAsync(u => u.AvatarImageId == imageId || u.BannerImageId == imageId, ct)
            || await db.Server.AnyAsync(s => s.IconImageId == imageId, ct)
            || await db.Roles.AnyAsync(r => r.IconImageId == imageId, ct);

    private static IQueryable<Data.Image> Unreferenced(AppDbContext db)
        => db.Images.Where(i =>
            !db.Users.Any(u => u.AvatarImageId == i.Id || u.BannerImageId == i.Id)
            && !db.Server.Any(s => s.IconImageId == i.Id)
            && !db.Roles.Any(r => r.IconImageId == i.Id));

    private static ImageRecord ToRecord(Data.Image row)
        => new(row.Id, row.Purpose, row.UploaderId, row.ContentType, row.Size);

    private IEnumerable<string> EnumerateFiles(long id)
    {
        try
        {
            // Matches the abandoned .part of the same id too, which is equally disk to reclaim.
            return Directory.EnumerateFiles(Root, $"{id}.*").ToList();
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or ArgumentException)
        {
            logger.LogDebug("Could not list image files for {ImageId} ({Reason})", id, ex.GetType().Name);
            return [];
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
            logger.LogDebug("Could not delete image file {Path} ({Reason})", path, ex.GetType().Name);
        }
    }
}
