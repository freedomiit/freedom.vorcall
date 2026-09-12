namespace Vorcall.Server.Logging;

// Where the JSON log files are written. In production this is the bind mount docker-compose
// points at ./logs on the host. A directory the process cannot create fails the boot like the
// other Vorcall options: a server whose only record of what it did is a container's stdout is
// one nobody can answer a question about an hour later.
public sealed record LoggingOptions
{
    public const string DefaultLogsDir = "/logs";

    private LoggingOptions(string logsDir) => LogsDir = logsDir;

    public string LogsDir { get; }

    public static LoggingOptions FromConfiguration(IConfiguration configuration)
    {
        var configured = configuration["Vorcall:LogsDir"]?.Trim();
        return new LoggingOptions(string.IsNullOrEmpty(configured) ? DefaultLogsDir : configured);
    }

    // Called before the sink opens its first file, so a missing mount or a read-only one is a
    // boot failure with a reason rather than a sink that silently drops every line.
    public string EnsureDirectory()
    {
        try
        {
            Directory.CreateDirectory(LogsDir);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException)
        {
            throw new InvalidOperationException($"Cannot create the log directory '{LogsDir}': {ex.Message}", ex);
        }

        return LogsDir;
    }
}
