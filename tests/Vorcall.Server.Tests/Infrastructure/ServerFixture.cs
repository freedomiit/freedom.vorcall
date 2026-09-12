using System.Globalization;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using Npgsql;
using Xunit;

namespace Vorcall.Server.Tests.Infrastructure;

// The suite's servers and the databases behind them. Every factory gets a database of its own,
// created here and dropped with FORCE on the way out; nothing ever touches the development
// database the compose file serves.
public sealed class ServerFixture : IAsyncLifetime
{
    public const string ServerKey = "test-key";
    public const string AdminKey = "test-admin";

    // The dedicated factories' limits: low enough for a test to trip them in a second.
    public const int TightMessageBurst = 3;
    public const int TightDiagnosticsPerHour = 2;
    public const int TightAuthPerWindow = 3;

    private const string DefaultAdminConnectionString =
        "Host=localhost;Port=5433;Username=vorcall;Password=vorcall;Database=postgres";

    private readonly string _adminConnectionString =
        Environment.GetEnvironmentVariable("VORCALL_TEST_ADMIN") ?? DefaultAdminConnectionString;

    private readonly List<string> _databases = [];
    private readonly List<VorcallFactory> _factories = [];
    private readonly Dictionary<string, VorcallFactory> _named = new(StringComparer.Ordinal);
    private readonly SemaphoreSlim _gate = new(1, 1);
    private readonly string _root = Path.Combine(
        Path.GetTempPath(),
        "vorcall-tests",
        Guid.NewGuid().ToString("N")[..8]);

    public VorcallFactory Server { get; private set; } = null!;

    public async Task InitializeAsync()
    {
        await EnsurePostgresAsync();
        Server = await BuildAsync(null);
    }

    // Screen sharing off, voice on.
    public Task<VorcallFactory> ShareDisabledAsync() => NamedAsync(
        "share-off",
        settings => settings["Vorcall:ShareEnabled"] = "false");

    // No relay at all: JoinVoice answers VOICE_UNAVAILABLE.
    public Task<VorcallFactory> VoiceDisabledAsync() => NamedAsync(
        "voice-off",
        settings => settings["Vorcall:VoiceEnabled"] = "false");

    // The write ceiling and the diagnostics ceiling, both turned down to something a test can
    // reach without waiting. The refill rate is close enough to zero that a bucket emptied by
    // one test stays empty for the rest of that test.
    public Task<VorcallFactory> TightLimitsAsync() => NamedAsync("tight-limits", settings =>
    {
        settings["Vorcall:MessageBurst"] = TightMessageBurst.ToString(CultureInfo.InvariantCulture);
        settings["Vorcall:MessagesPerSecond"] = "0.001";
        settings["Vorcall:DiagnosticsReportsPerHour"] = TightDiagnosticsPerHour.ToString(CultureInfo.InvariantCulture);
    });

    // Its own host because the per-IP window is shared by every request a host answers: one test
    // deliberately exhausts it.
    public Task<VorcallFactory> TightAuthAsync() => NamedAsync(
        "tight-auth",
        settings => settings["Vorcall:AuthRequestsPerWindow"] = TightAuthPerWindow.ToString(CultureInfo.InvariantCulture));

    public async Task DisposeAsync()
    {
        foreach (var factory in _factories)
        {
            await factory.DisposeAsync();
        }

        // The hosts are down but their pools are not: a pooled connection would keep the
        // database alive even against a forced drop.
        NpgsqlConnection.ClearAllPools();

        foreach (var database in _databases)
        {
            await ExecuteAdminAsync($"DROP DATABASE IF EXISTS \"{database}\" WITH (FORCE)");
        }

        try
        {
            if (Directory.Exists(_root))
            {
                Directory.Delete(_root, recursive: true);
            }
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // Temp files the operating system will reclaim; never a reason to fail a run.
        }

        _gate.Dispose();
    }

    private async Task<VorcallFactory> NamedAsync(string name, Action<IDictionary<string, string>> overrides)
    {
        await _gate.WaitAsync();
        try
        {
            if (_named.TryGetValue(name, out var existing))
            {
                return existing;
            }

            var factory = await BuildAsync(overrides);
            _named[name] = factory;
            return factory;
        }
        finally
        {
            _gate.Release();
        }
    }

    private async Task<VorcallFactory> BuildAsync(Action<IDictionary<string, string>>? overrides)
    {
        var id = Guid.NewGuid().ToString("N")[..8];
        var database = $"vorcall_test_{id}";
        await ExecuteAdminAsync($"CREATE DATABASE \"{database}\"");
        _databases.Add(database);

        var settings = new Dictionary<string, string>(StringComparer.Ordinal)
        {
            ["ConnectionStrings:Default"] = ConnectionStringFor(database),
            ["Vorcall:ServerKey"] = ServerKey,
            ["Vorcall:JwtSigningKey"] = Convert.ToBase64String(RandomNumberGenerator.GetBytes(32)),
            ["Vorcall:VoicePort"] = FreeUdpPort().ToString(CultureInfo.InvariantCulture),
            ["Vorcall:VoiceHost"] = "127.0.0.1",
            ["Vorcall:LogsDir"] = Directory.CreateDirectory(Path.Combine(_root, id, "logs")).FullName,
            ["Vorcall:DiagnosticsDir"] = Directory.CreateDirectory(Path.Combine(_root, id, "diagnostics")).FullName,
            ["Vorcall:AttachmentsDir"] = Directory.CreateDirectory(Path.Combine(_root, id, "attachments")).FullName,
            ["Vorcall:ReleasesDir"] = Directory.CreateDirectory(Path.Combine(_root, id, "releases")).FullName,
            ["Vorcall:AdminKey"] = AdminKey,

            // High enough that only a dedicated factory's own low limit is ever the one a test
            // trips: the in-process host gives every request the same "unknown" client address,
            // so the whole suite shares one per-IP partition.
            ["Vorcall:AuthRequestsPerWindow"] = "100000",
            ["Vorcall:UploadRequestsPerWindow"] = "100000",
            ["Vorcall:MessageBurst"] = "100000",
            ["Vorcall:MessagesPerSecond"] = "100000",
            ["Vorcall:DiagnosticsReportsPerHour"] = "100000",
        };

        overrides?.Invoke(settings);
        Releases.Publish(settings["Vorcall:ReleasesDir"]);

        var factory = new VorcallFactory(settings);
        _factories.Add(factory);

        // Starts the host here rather than inside the first test that touches it, so a boot
        // failure names the fixture instead of an unrelated assertion.
        _ = factory.Server;
        return factory;
    }

    private string ConnectionStringFor(string database)
        => new NpgsqlConnectionStringBuilder(_adminConnectionString) { Database = database }.ConnectionString;

    private async Task ExecuteAdminAsync(string sql)
    {
        await using var connection = new NpgsqlConnection(_adminConnectionString);
        await connection.OpenAsync();
        await using var command = new NpgsqlCommand(sql, connection);
        await command.ExecuteNonQueryAsync();
    }

    private async Task EnsurePostgresAsync()
    {
        try
        {
            await using var connection = new NpgsqlConnection(_adminConnectionString);
            await connection.OpenAsync();
        }
        catch (Exception ex) when (ex is NpgsqlException or SocketException or TimeoutException)
        {
            var target = new NpgsqlConnectionStringBuilder(_adminConnectionString);
            throw new InvalidOperationException(
                $"PostgreSQL is not reachable at {target.Host}:{target.Port}. Start it with "
                + "`docker compose up -d db`, or point VORCALL_TEST_ADMIN at another server.",
                ex);
        }
    }

    // The relay binds this port for real, so two hosts may not share one. Bound and released
    // here only to have the kernel name a port nothing else holds.
    private static int FreeUdpPort()
    {
        using var probe = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
        probe.Bind(new IPEndPoint(IPAddress.Any, 0));
        return ((IPEndPoint)probe.LocalEndPoint!).Port;
    }
}
