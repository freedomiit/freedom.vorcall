using Serilog;
using Serilog.Formatting.Compact;

namespace Vorcall.Server.Logging;

// Structured logging for the web path only. The admin CLI keeps the default console provider
// (AdminCli.RunAsync), so "docker compose run --rm backend invites new" never opens the log file
// the running server is writing to.
public static class LoggingSetup
{
    // One file a day, a month of history: enough to answer "what happened last night" without
    // letting the directory grow without bound on a small host.
    private const int RetainedFileCount = 31;

    // SessionId is empty outside a connection scope; inside one, every line of that session
    // carries it, which is what makes the console followable with several friends connected.
    private const string ConsoleTemplate = "[{Timestamp:HH:mm:ss} {Level:u3}] {SessionId} {Message:lj}{NewLine}{Exception}";

    // The file sink buffers; anything newer than this is lost if the process dies outright,
    // which is the trade for not paying a disk flush per line.
    private static readonly TimeSpan FlushInterval = TimeSpan.FromSeconds(2);

    public static void Configure(WebApplicationBuilder builder)
    {
        // `UseSerilog` below only takes effect once the host is built, and until then the static
        // logger is Serilog's silent default. Anything logged while the services are still being
        // wired — SecretStore printing a generated door key, an option refusing a value — would
        // vanish without this. Console only: the file sink starts with the real logger, and the
        // door key is better off not being written to disk. `UseSerilog` retires it.
        Log.Logger = new LoggerConfiguration()
            .WriteTo.Console(outputTemplate: ConsoleTemplate)
            .CreateBootstrapLogger();

        var logsDir = LoggingOptions.FromConfiguration(builder.Configuration).EnsureDirectory();

        builder.Host.UseSerilog((context, services, configuration) => configuration
            .ReadFrom.Configuration(context.Configuration)
            .Enrich.FromLogContext()
            .Enrich.WithProperty("Application", "vorcall")
            .WriteTo.Console(outputTemplate: ConsoleTemplate)
            .WriteTo.File(
                new CompactJsonFormatter(),
                Path.Combine(logsDir, "vorcall-.json"),
                rollingInterval: RollingInterval.Day,
                retainedFileCountLimit: RetainedFileCount,
                shared: false,
                flushToDiskInterval: FlushInterval));
    }
}
