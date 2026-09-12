using System.Text;

namespace Vorcall.Server.Diagnostics;

public enum StoreResult
{
    Stored,
    TooLarge,
    NotText,
    InvalidName,
    InvalidKind,

    // The upload was fine and this server could not write it down.
    StorageFailed,
}

// Log files and crash reports, as files on the owner's disk and nothing else: there is no row,
// no listing endpoint and no path a client controls. Everything a client sends is reduced to
// [A-Za-z0-9._-] before it reaches a file name, and the body is accepted only if it is UTF-8
// text — a diagnostics directory is read with a terminal, not with a sandbox.
public sealed class DiagnosticsStore(DiagnosticsOptions options, ILogger<DiagnosticsStore> logger)
{
    public const int MaxNameLength = 64;

    private const int CopyBufferBytes = 64 * 1024;
    private const string PartSuffix = ".part";

    private static readonly string[] Kinds = ["log", "crash"];

    public async Task<StoreResult> StoreAsync(
        long userId,
        string username,
        string kind,
        string fileName,
        Stream body,
        CancellationToken ct)
    {
        if (Array.IndexOf(Kinds, kind) < 0)
        {
            return StoreResult.InvalidKind;
        }

        if (!TrySanitize(fileName, out var safeName))
        {
            return StoreResult.InvalidName;
        }

        // The account's own name is no more trusted than the file's; a user with no name at all
        // still gets a readable stored name out of the id and the timestamp.
        var safeUser = TrySanitize(username, out var cleaned) ? cleaned : "user";
        var stamp = DateTime.UtcNow.ToString("yyyyMMdd'T'HHmmss'Z'");
        var path = PathFor($"{userId}-{safeUser}-{stamp}-{kind}-{safeName}");
        var partPath = $"{path}.{Path.GetRandomFileName()}{PartSuffix}";

        StoreResult? failure = null;
        var written = 0L;
        try
        {
            Directory.CreateDirectory(options.Dir);

            // UTF-8 sequences straddle chunk boundaries, so validation is a decoder carried
            // across the whole body rather than a per-chunk check.
            var decoder = new UTF8Encoding(false, true).GetDecoder();
            var chars = new char[Encoding.UTF8.GetMaxCharCount(CopyBufferBytes)];

            // Written under a .part name unique to this upload and renamed at the end, so a file
            // under the final name is always a complete report, and two uploads of one name in
            // the same second cannot trip over each other's in-progress file.
            await using (var file = new FileStream(partPath, FileMode.Create, FileAccess.Write, FileShare.None))
            {
                var buffer = new byte[CopyBufferBytes];
                while (true)
                {
                    var read = await body.ReadAsync(buffer, ct);
                    if (read == 0)
                    {
                        break;
                    }

                    written += read;
                    if (written > DiagnosticsOptions.MaxFileBytes)
                    {
                        failure = StoreResult.TooLarge;
                        break;
                    }

                    if (HasBinaryByte(buffer.AsSpan(0, read)))
                    {
                        failure = StoreResult.NotText;
                        break;
                    }

                    try
                    {
                        decoder.GetChars(buffer, 0, read, chars, 0, flush: false);
                    }
                    catch (DecoderFallbackException)
                    {
                        failure = StoreResult.NotText;
                        break;
                    }

                    await file.WriteAsync(buffer.AsMemory(0, read), ct);
                }

                if (failure is null)
                {
                    try
                    {
                        // A body ending mid-sequence is caught here and nowhere else.
                        decoder.GetChars([], 0, 0, chars, 0, flush: true);
                    }
                    catch (DecoderFallbackException)
                    {
                        failure = StoreResult.NotText;
                    }
                }
            }

            if (failure is null)
            {
                File.Move(partPath, path, overwrite: true);
            }
        }
        catch (OperationCanceledException)
        {
            logger.LogDebug("Diagnostics upload by {UserId} aborted", userId);
            failure = StoreResult.StorageFailed;
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // The caller sent a perfectly good report and the disk refused it: this is the
            // server failing, and the only line that says so.
            logger.LogError(ex, "Diagnostics report from {UserId} could not be written to storage", userId);
            failure = StoreResult.StorageFailed;
        }

        if (failure is { } rejected)
        {
            TryDeleteFile(partPath);
            return rejected;
        }

        // Name and size only: a report's contents never reach a log line.
        logger.LogInformation(
            "Diagnostics {Kind} report stored for {UserId} as {StoredName} ({SizeBytes} bytes)",
            kind,
            userId,
            Path.GetFileName(path),
            written);
        return StoreResult.Stored;
    }

    // What the sweeper walks. Missing directory means nothing has been uploaded yet.
    public IEnumerable<FileInfo> Files()
    {
        var directory = new DirectoryInfo(options.Dir);
        return directory.Exists ? directory.EnumerateFiles() : [];
    }

    // Only ever called with a name already reduced to the safe alphabet; the assertion is what
    // keeps it that way.
    private string PathFor(string storedName)
    {
        var root = Path.TrimEndingDirectorySeparator(Path.GetFullPath(options.Dir));
        var path = Path.GetFullPath(Path.Combine(root, storedName));

        if (Path.GetDirectoryName(path) != root)
        {
            throw new InvalidOperationException("Diagnostics path escaped the diagnostics directory.");
        }

        return path;
    }

    // Anything outside [A-Za-z0-9._-] becomes an underscore, so no separator, no control
    // character and no dot-dot survives. A leading dot is refused rather than rewritten: a
    // hidden file in a directory the owner reads with ls is a report nobody sees.
    internal static bool TrySanitize(string? raw, out string safe)
    {
        safe = string.Empty;
        if (string.IsNullOrWhiteSpace(raw))
        {
            return false;
        }

        var builder = new StringBuilder(MaxNameLength);
        foreach (var c in raw.Trim())
        {
            if (builder.Length == MaxNameLength)
            {
                break;
            }

            builder.Append(char.IsAsciiLetterOrDigit(c) || c is '.' or '_' or '-' ? c : '_');
        }

        var name = builder.ToString();
        if (name.Length == 0 || name[0] == '.')
        {
            return false;
        }

        safe = name;
        return true;
    }

    private static bool HasBinaryByte(ReadOnlySpan<byte> chunk)
    {
        foreach (var b in chunk)
        {
            if (b < 0x20 && b is not ((byte)'\t' or (byte)'\n' or (byte)'\r'))
            {
                return true;
            }
        }

        return false;
    }

    private void TryDeleteFile(string path)
    {
        try
        {
            File.Delete(path);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or NotSupportedException or ArgumentException)
        {
            logger.LogDebug("Could not delete diagnostics file {Path} ({Reason})", path, ex.GetType().Name);
        }
    }
}
