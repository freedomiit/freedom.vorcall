using System.Globalization;

namespace Vorcall.Server.Diagnostics;

// Where the log files and crash reports clients upload are kept, and how many a single account
// may send in an hour. In production the directory is the bind mount docker-compose.prod.yml
// points at ./diagnostics on the host; an unusable per-hour limit fails the boot like the other
// Vorcall options rather than silently falling back to the default.
public sealed record DiagnosticsOptions
{
    public const string DefaultDir = "/diagnostics";

    // One report is up to two log files plus five crash reports, each its own request.
    public const int DefaultReportsPerHour = 10;

    // Per file: above this the upload is refused with 413.
    public const int MaxFileBytes = 4 << 20;

    // A report nobody has read in a month is disk to reclaim, not evidence.
    public static readonly TimeSpan Retention = TimeSpan.FromDays(30);

    private DiagnosticsOptions(string dir, int reportsPerHour)
    {
        Dir = dir;
        ReportsPerHour = reportsPerHour;
    }

    public string Dir { get; }

    public int ReportsPerHour { get; }

    public static DiagnosticsOptions FromConfiguration(IConfiguration configuration)
    {
        var dir = configuration["Vorcall:DiagnosticsDir"]?.Trim();
        return new DiagnosticsOptions(
            string.IsNullOrEmpty(dir) ? DefaultDir : dir,
            ParseReportsPerHour(configuration["Vorcall:DiagnosticsReportsPerHour"]));
    }

    private static int ParseReportsPerHour(string? configured)
    {
        if (string.IsNullOrWhiteSpace(configured))
        {
            return DefaultReportsPerHour;
        }

        if (!int.TryParse(configured.Trim(), NumberStyles.Integer, CultureInfo.InvariantCulture, out var perHour) || perHour < 1)
        {
            throw new InvalidOperationException("Invalid configuration 'Vorcall:DiagnosticsReportsPerHour' (expected a count of at least 1).");
        }

        return perHour;
    }
}
