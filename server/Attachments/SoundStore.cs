using System.Buffers.Binary;
using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Chat;
using Vorcall.Server.Data;
using Vorcall.Server.Metrics;

namespace Vorcall.Server.Attachments;

public sealed record SoundRecord(long Id, string Name, long? UploaderId, string ContentType, long Size, int DurationMs);

// Sound is set only when Status is Saved.
public sealed record SoundSaveOutcome(SoundSaveOutcome.Kind Status, SoundRecord? Sound)
{
    public enum Kind
    {
        Saved,
        TooLarge,

        // The VORCSND1 container is not what PROTOCOL.md § Sounds describes.
        Malformed,

        // The name is not one the 32-scalar grammar accepts; the container says nothing about it.
        InvalidName,
        Truncated,
        QuotaExceeded,

        // The upload was fine and this server could not write it down.
        IoError,
    }

    public static SoundSaveOutcome Rejected(Kind kind) => new(kind, null);

    public static SoundSaveOutcome Saved(SoundRecord sound) => new(Kind.Saved, sound);
}

// The server-wide soundpad library: the same streaming upload and the same storage quota as
// attachments and images, in a store of its own under sounds/, holding a clip to the VORCSND1
// container of PROTOCOL.md § Sounds. The container is walked while it streams and never decoded,
// which is what makes a clip's duration known from its frame count alone.
public sealed class SoundStore(
    AttachmentsOptions options,
    IDbContextFactory<AppDbContext> contexts,
    ServerMetrics metrics,
    ILogger<SoundStore> logger)
{
    public const string SubDirectory = "sounds";

    // Every clip is stored under its row id and this one extension, like an attachment: the name
    // a member gave it is metadata and never part of a path.
    public const string StoredExtension = "vcsnd";

    private const int CopyBufferBytes = 64 * 1024;
    private const string PartSuffix = ".part";

    // Every container frame is one 20 ms Opus packet, which is what makes the duration a count.
    private const int FrameMs = 20;

    private string Root => Path.Combine(Path.TrimEndingDirectorySeparator(Path.GetFullPath(options.Dir)), SubDirectory);

    public string PathFor(long id)
    {
        var root = Root;
        var path = Path.GetFullPath(Path.Combine(root, $"{id}.{StoredExtension}"));

        // Ids are numbers and the extension is a literal, so there is no traversal to filter out;
        // this is the assertion that keeps it that way.
        if (Path.GetDirectoryName(path) != root)
        {
            throw new InvalidOperationException("Sound path escaped the sounds directory.");
        }

        return path;
    }

    // The quota is the attachments' (Vorcall:AttachmentsMaxBytes) and shared with them, so all
    // three tables count here, in ImageStore and in AttachmentStore alike.
    public async Task<long> TotalBytesAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var sounds = await db.Sounds.SumAsync(s => (long?)s.Size, ct) ?? 0;
        var images = await db.Images.SumAsync(i => (long?)i.Size, ct) ?? 0;
        var attachments = await db.Attachments.SumAsync(a => (long?)a.Size, ct) ?? 0;
        return sounds + images + attachments;
    }

    public async Task<SoundSaveOutcome> SaveAsync(
        string name,
        long uploaderId,
        long declaredLength,
        Stream body,
        CancellationToken ct)
    {
        if (!Names.TryNormalize(name, out var normalized))
        {
            return SoundSaveOutcome.Rejected(SoundSaveOutcome.Kind.InvalidName);
        }

        if (declaredLength > AttachmentsOptions.SoundMaxFileBytes)
        {
            return SoundSaveOutcome.Rejected(SoundSaveOutcome.Kind.TooLarge);
        }

        var storedBytes = await TotalBytesAsync(ct);
        if (storedBytes + declaredLength > options.MaxBytes)
        {
            logger.LogWarning(
                "Sound upload by {UserId} refused: {StoredBytes} stored plus {DeclaredBytes} declared exceeds the {QuotaBytes} quota",
                uploaderId,
                storedBytes,
                declaredLength,
                options.MaxBytes);
            return SoundSaveOutcome.Rejected(SoundSaveOutcome.Kind.QuotaExceeded);
        }

        await using var db = await contexts.CreateDbContextAsync(ct);

        // The row is written first because the id names the file, and stays incomplete until the
        // bytes are under that name; the duration is only knowable once the container's header has
        // arrived, so it is filled in at the end.
        var row = new Sound
        {
            Name = normalized,
            UploaderId = uploaderId,
            ContentType = AttachmentsOptions.SoundMediaType,
            Size = declaredLength,
            DurationMs = 0,
            Complete = false,
            CreatedAt = DateTime.UtcNow,
        };

        db.Sounds.Add(row);
        await db.SaveChangesAsync(ct);

        var path = PathFor(row.Id);
        var partPath = path + PartSuffix;
        SoundSaveOutcome.Kind? failure = null;
        try
        {
            Directory.CreateDirectory(Root);

            var container = new ContainerValidator();
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
                        logger.LogDebug("Sound {SoundId} upload aborted", row.Id);
                        failure = SoundSaveOutcome.Kind.Truncated;
                        break;
                    }

                    if (read == 0)
                    {
                        break;
                    }

                    written += read;
                    if (written > declaredLength)
                    {
                        failure = SoundSaveOutcome.Kind.TooLarge;
                        break;
                    }

                    if (!container.Feed(buffer.AsSpan(0, read)))
                    {
                        failure = SoundSaveOutcome.Kind.Malformed;
                        break;
                    }

                    await file.WriteAsync(buffer.AsMemory(0, read), ct);
                }
            }

            // A body that stopped anywhere but exactly on the last frame's last byte is not this
            // container, however far it got.
            if (failure is null && !container.Complete)
            {
                failure = SoundSaveOutcome.Kind.Malformed;
            }

            if (failure is null && written != declaredLength)
            {
                failure = SoundSaveOutcome.Kind.Truncated;
            }

            if (failure is null)
            {
                row.DurationMs = container.Frames * FrameMs;
                File.Move(partPath, path, overwrite: true);
            }
        }
        catch (OperationCanceledException)
        {
            logger.LogDebug("Sound {SoundId} upload aborted", row.Id);
            failure = SoundSaveOutcome.Kind.Truncated;
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // The caller sent a perfectly good clip and the disk refused it, so this is not a
            // rejected upload: it is this server failing, and the only line that says so.
            logger.LogError(ex, "Sound {SoundId} could not be written to storage", row.Id);
            failure = SoundSaveOutcome.Kind.IoError;
        }

        if (failure is { } rejected)
        {
            TryDeleteFile(partPath);

            // Not ct: a cancelled upload still has to take its row with it.
            db.Sounds.Remove(row);
            await db.SaveChangesAsync(CancellationToken.None);
            return SoundSaveOutcome.Rejected(rejected);
        }

        // Only after the rename, so the flag is true exactly when the bytes are on disk under the
        // final name, and the duration goes down with it. Not ct, for the same reason the removal
        // above is not.
        row.Complete = true;
        await db.SaveChangesAsync(CancellationToken.None);

        logger.LogDebug(
            "Sound {SoundId} stored for {UserId} ({SizeBytes} bytes, {DurationMs} ms)",
            row.Id,
            uploaderId,
            row.Size,
            row.DurationMs);
        metrics.CountSoundUpload();
        return SoundSaveOutcome.Saved(ToRecord(row));
    }

    // The row and the file it names, for the download endpoint to stream; null when the id names
    // no sound. An incomplete row is not one: its bytes are not under that name yet.
    public async Task<(SoundRecord Sound, string Path)?> GetAsync(long id, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var row = await db.Sounds.AsNoTracking().FirstOrDefaultAsync(s => s.Id == id && s.Complete, ct);
        return row is null ? null : (ToRecord(row), PathFor(row.Id));
    }

    // The whole library: it is server-wide and every member sees all of it. Incomplete rows are
    // uploads still in flight and are no part of it.
    public async Task<List<SoundRecord>> ListAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var rows = await db.Sounds.AsNoTracking().Where(s => s.Complete).OrderBy(s => s.Id).ToListAsync(ct);
        return rows.ConvertAll(ToRecord);
    }

    public async Task<bool> RenameAsync(long id, string name, CancellationToken ct)
    {
        if (!Names.TryNormalize(name, out var normalized))
        {
            return false;
        }

        await using var db = await contexts.CreateDbContextAsync(ct);
        var row = await db.Sounds.FirstOrDefaultAsync(s => s.Id == id, ct);
        if (row is null)
        {
            return false;
        }

        row.Name = normalized;
        await db.SaveChangesAsync(ct);
        return true;
    }

    // Row and file together: unlike an image, a clip is referenced by nothing, so a delete is the
    // only thing that ever removes one.
    public async Task<bool> DeleteAsync(long id, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var deleted = await db.Sounds.Where(s => s.Id == id).ExecuteDeleteAsync(ct);
        if (deleted == 0)
        {
            return false;
        }

        DeleteFiles(id);
        return true;
    }

    // An upload whose bytes never finished arriving: the process died between the row and the
    // rename, and nothing else will ever complete it. Unlike an attachment this uses the short
    // unlinked cutoff rather than IncompleteTtl — a clip is capped at 16 MiB, which cannot take an
    // hour to arrive the way a 2 GiB attachment can.
    public async Task<int> SweepIncompleteAsync(DateTime olderThan, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var stale = await db.Sounds
            .AsNoTracking()
            .Where(s => !s.Complete && s.CreatedAt < olderThan)
            .Select(s => s.Id)
            .ToListAsync(ct);
        if (stale.Count == 0)
        {
            return 0;
        }

        var deleted = await db.Sounds
            .Where(s => stale.Contains(s.Id) && !s.Complete)
            .ExecuteDeleteAsync(ct);

        // Anything still there completed between the read and the delete, and its file is now the
        // library's.
        var survivors = await db.Sounds
            .AsNoTracking()
            .Where(s => stale.Contains(s.Id))
            .Select(s => s.Id)
            .ToListAsync(ct);

        foreach (var id in stale.Where(id => !survivors.Contains(id)))
        {
            DeleteFiles(id);
        }

        logger.LogDebug("Swept {Count} incomplete sounds older than {Cutoff}", deleted, olderThan);
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
            logger.LogDebug("Could not list sound files ({Reason})", ex.GetType().Name);
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
        var known = await db.Sounds
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
            logger.LogDebug("Swept {Count} orphaned sound files", deleted);
        }

        return deleted;
    }

    private static SoundRecord ToRecord(Sound row)
        => new(row.Id, row.Name, row.UploaderId, row.ContentType, row.Size, row.DurationMs);

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
            logger.LogDebug("Could not list sound files for {SoundId} ({Reason})", id, ex.GetType().Name);
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
            logger.LogDebug("Could not delete sound file {Path} ({Reason})", path, ex.GetType().Name);
        }
    }

    // Walks the VORCSND1 container of PROTOCOL.md § Sounds as the body streams past, one buffer at
    // a time and without ever looking inside an Opus packet: header, then frames x (u16 length,
    // that many bytes), landing exactly on the end of the body.
    private sealed class ContainerValidator
    {
        private const int SampleRate = 48000;
        private const byte Channels = 2;

        private readonly byte[] _header = new byte[AttachmentsOptions.SoundHeaderBytes];
        private readonly byte[] _length = new byte[2];

        private Stage _stage = Stage.Header;
        private int _headerLength;
        private int _lengthLength;
        private int _remainingFrames;
        private int _remainingBody;

        private enum Stage
        {
            Header,
            Length,
            Body,
            Done,
        }

        public int Frames { get; private set; }

        public bool Complete => _stage == Stage.Done;

        public bool Feed(ReadOnlySpan<byte> chunk)
        {
            while (!chunk.IsEmpty)
            {
                switch (_stage)
                {
                    case Stage.Header:
                        var headerCopied = Math.Min(_header.Length - _headerLength, chunk.Length);
                        chunk[..headerCopied].CopyTo(_header.AsSpan(_headerLength));
                        _headerLength += headerCopied;
                        chunk = chunk[headerCopied..];
                        if (_headerLength < _header.Length)
                        {
                            return true;
                        }

                        if (!ReadHeader())
                        {
                            return false;
                        }

                        _stage = Stage.Length;
                        break;

                    case Stage.Length:
                        var lengthCopied = Math.Min(_length.Length - _lengthLength, chunk.Length);
                        chunk[..lengthCopied].CopyTo(_length.AsSpan(_lengthLength));
                        _lengthLength += lengthCopied;
                        chunk = chunk[lengthCopied..];
                        if (_lengthLength < _length.Length)
                        {
                            return true;
                        }

                        var packet = BinaryPrimitives.ReadUInt16LittleEndian(_length);
                        if (packet is 0 || packet > AttachmentsOptions.SoundMaxPacketBytes)
                        {
                            return false;
                        }

                        _lengthLength = 0;
                        _remainingBody = packet;
                        _stage = Stage.Body;
                        break;

                    case Stage.Body:
                        var skipped = Math.Min(_remainingBody, chunk.Length);
                        _remainingBody -= skipped;
                        chunk = chunk[skipped..];
                        if (_remainingBody > 0)
                        {
                            return true;
                        }

                        _remainingFrames--;
                        _stage = _remainingFrames == 0 ? Stage.Done : Stage.Length;
                        break;

                    default:
                        // Done, and a byte after the last frame: trailing bytes are not this
                        // container either.
                        return false;
                }
            }

            return true;
        }

        private bool ReadHeader()
        {
            var header = _header.AsSpan();
            if (!header[..8].SequenceEqual(AttachmentsOptions.SoundMagic)
                || BinaryPrimitives.ReadUInt32LittleEndian(header[8..12]) != SampleRate
                || header[12] != Channels
                || header[13] != 0)
            {
                return false;
            }

            var frames = BinaryPrimitives.ReadUInt32LittleEndian(header[14..18]);
            if (frames is 0 || frames > AttachmentsOptions.SoundMaxFrames)
            {
                return false;
            }

            Frames = (int)frames;
            _remainingFrames = Frames;
            return true;
        }
    }
}
