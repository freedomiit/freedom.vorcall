using System.Globalization;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;

namespace Vorcall.Server.Tests.Infrastructure;

// One in-process server: its own database, its own temp directories, its own UDP port for the
// relay. Every setting reaches the host as a command line argument (that is what
// WebApplicationFactory does with UseSetting on a minimal-API app), which is the highest
// precedence configuration source, so nothing in appsettings.json can reach a test.
public sealed class VorcallFactory(IReadOnlyDictionary<string, string> settings) : WebApplicationFactory<global::Program>
{
    static VorcallFactory()
    {
        // The content root is otherwise derived from a manifest keyed on the test host's current
        // directory, with a solution-file search as the fallback; this repository has no solution
        // file. The test output directory carries the server's appsettings.json, so it is a
        // content root the server is happy with, and the setting is read from this variable.
        Environment.SetEnvironmentVariable("ASPNETCORE_TEST_CONTENTROOT_VORCALL_SERVER", AppContext.BaseDirectory);
    }

    private HttpClient? _client;

    public HttpClient Client => _client ??= CreateClient();

    public string ServerKey => Setting("Vorcall:ServerKey");

    public string LogsDir => Setting("Vorcall:LogsDir");

    public string DiagnosticsDir => Setting("Vorcall:DiagnosticsDir");

    public string AttachmentsDir => Setting("Vorcall:AttachmentsDir");

    public string ReleasesDir => Setting("Vorcall:ReleasesDir");

    public int VoicePort => int.Parse(Setting("Vorcall:VoicePort"), CultureInfo.InvariantCulture);

    public string Setting(string key) => settings[key];

    protected override void ConfigureWebHost(IWebHostBuilder builder)
    {
        // Production, so appsettings.Development.json - whose connection string is the developer's
        // own database - is never loaded by a test run.
        builder.UseEnvironment("Production");
        foreach (var (key, value) in settings)
        {
            builder.UseSetting(key, value);
        }
    }
}
