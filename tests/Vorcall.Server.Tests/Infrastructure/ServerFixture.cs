using System.Globalization;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using Microsoft.AspNetCore.Identity;
using Microsoft.EntityFrameworkCore;
using Npgsql;
using Vorcall.Server.Data;
using Vorcall.Server.Protocol;
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
    public const int TightStreamTimeoutSeconds = 1;
    public const int TightCamerasPerRoom = 2;

    private const string DefaultAdminConnectionString =
        "Host=localhost;Port=5433;Username=vorcall;Password=vorcall;Database=postgres";

    private static readonly TimeSpan OwnerTokenFreshness = TimeSpan.FromMinutes(10);

    private readonly string _adminConnectionString =
        Environment.GetEnvironmentVariable("VORCALL_TEST_ADMIN") ?? DefaultAdminConnectionString;

    private readonly List<string> _databases = [];
    private readonly List<VorcallFactory> _factories = [];
    private readonly Dictionary<string, VorcallFactory> _named = new(StringComparer.Ordinal);
    private readonly Dictionary<VorcallFactory, (Account Owner, DateTime MintedAt)> _owners = [];
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

    // The host's owner account, signed in. The owner is the one bypass of every permission check,
    // so it is the actor of every test about managing channels, categories, roles, overrides or
    // members; @everyone grants none of those. One account per host, written before the host booted
    // (see SeedOwnerAsync), signed in on first use so a factory whose auth limit a test exhausts
    // does not spend a request on a login it never makes. The pair is re-minted well inside the
    // access token's 15 minutes, because a whole suite run is longer than that.
    internal async Task<Account> OwnerAsync(VorcallFactory factory)
    {
        await _gate.WaitAsync();
        try
        {
            var now = DateTime.UtcNow;
            if (_owners.TryGetValue(factory, out var cached) && now - cached.MintedAt < OwnerTokenFreshness)
            {
                return cached.Owner;
            }

            var response = await Accounts.LoginRawAsync(factory, Accounts.OwnerUsername, Accounts.Password);
            Assert.True(response.Status == HttpStatusCode.OK, $"owner login: {(int)response.Status}");
            var owner = Account.From(response.As(TokenResponse.Parser), Accounts.Password);
            _owners[factory] = (owner, now);
            return owner;
        }
        finally
        {
            _gate.Release();
        }
    }

    // Screen sharing off, voice on.
    public Task<VorcallFactory> ShareDisabledAsync() => NamedAsync(
        "share-off",
        settings => settings["Vorcall:ShareEnabled"] = "false");

    // Cameras off, voice and screen sharing on: the two kill switches are independent.
    public Task<VorcallFactory> CameraDisabledAsync() => NamedAsync(
        "camera-off",
        settings => settings["Vorcall:CameraEnabled"] = "false");

    // A channel ceiling of two cameras, which a party of three reaches without needing nine
    // accounts in one voice session.
    public Task<VorcallFactory> TightCamerasAsync() => NamedAsync(
        "tight-cameras",
        settings => settings["Vorcall:MaxCamerasPerRoom"] = TightCamerasPerRoom.ToString(CultureInfo.InvariantCulture));

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

    // A host whose server row has no owner, which is what a self-hosted install looks like between
    // `docker compose up -d` and the first account: the first registration takes ownership.
    public Task<VorcallFactory> UnownedAsync() => NamedAsync("unowned", _ => { }, seedOwner: false);

    // The streamed-file proxy's two ceilings, turned down to what a test can reach: a sender that
    // never answers is refused after a second rather than thirty, and one transfer per owner is a
    // cap a second reader trips at once. Its own host, and not the shared one, for both of them:
    // a one-second clock would race every honest push in the suite, and a cap of one would refuse
    // the second of two concurrent readers, which is a case of its own.
    public Task<VorcallFactory> TightStreamsAsync() => NamedAsync("tight-streams", settings =>
    {
        settings["Vorcall:StreamSenderTimeoutSeconds"] = TightStreamTimeoutSeconds.ToString(CultureInfo.InvariantCulture);
        settings["Vorcall:StreamMaxTransfersPerOwner"] = "1";
    });

    // Streamed files off: the four routes are never mapped.
    public Task<VorcallFactory> StreamsDisabledAsync() => NamedAsync(
        "streams-off",
        settings => settings["Vorcall:StreamsEnabled"] = "false");

    // A host of its own for the one test that stops it. Nothing else may share it, and nothing
    // else does: a stopped host answers no further request. The sender timeout stays at its
    // default, so the bound that test measures against is the shutdown and not a short clock.
    public Task<VorcallFactory> StoppableStreamsAsync() => NamedAsync("streams-stopping", _ => { });

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

    private async Task<VorcallFactory> NamedAsync(
        string name,
        Action<IDictionary<string, string>> overrides,
        bool seedOwner = true)
    {
        await _gate.WaitAsync();
        try
        {
            if (_named.TryGetValue(name, out var existing))
            {
                return existing;
            }

            var factory = await BuildAsync(overrides, seedOwner);
            _named[name] = factory;
            return factory;
        }
        finally
        {
            _gate.Release();
        }
    }

    private async Task<VorcallFactory> BuildAsync(
        Action<IDictionary<string, string>>? overrides,
        bool seedOwner = true)
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
        await SeedOwnerAsync(settings["ConnectionStrings:Default"], seedOwner);

        var factory = new VorcallFactory(settings);
        _factories.Add(factory);

        // Starts the host here rather than inside the first test that touches it, so a boot
        // failure names the fixture instead of an unrelated assertion.
        _ = factory.Server;
        return factory;
    }

    // Writes the account that owns the server, before the host that tests use has ever started. It
    // has to happen here: the registry reads the owner once at boot, so the admin CLI's
    // "server set-owner" takes effect only on the next start and a suite cannot restart its host.
    // Migrating and seeding here is what the host would have done at boot; its own Migrate and
    // EnsureSeededAsync then find the work already done.
    private static async Task SeedOwnerAsync(string connectionString, bool seedOwner = true)
    {
        var options = new DbContextOptionsBuilder<AppDbContext>().UseNpgsql(connectionString).Options;
        await using var db = new AppDbContext(options);
        await db.Database.MigrateAsync();

        // What a fresh self-hosted install looks like: migrated and seeded, but with no account
        // and so no owner. UnownedAsync's host boots from here.
        if (!seedOwner)
        {
            await Seed.EnsureSeededAsync(db, CancellationToken.None);
            return;
        }

        var owner = new User
        {
            Username = Accounts.OwnerUsername,
            UsernameNormalized = Accounts.OwnerUsername.ToUpperInvariant(),
            CreatedAt = DateTime.UtcNow,
        };

        // The server's own hasher, so POST /api/auth/login verifies this password like any other.
        owner.PasswordHash = new PasswordHasher<User>().HashPassword(owner, Accounts.Password);
        db.Users.Add(owner);
        await db.SaveChangesAsync();

        // Only for the fresh-install path Seed still owns: on a migrated database the server row
        // already exists, because the ChannelsRolesProfiles migration inserts it.
        await Seed.EnsureSeededAsync(db, CancellationToken.None);

        // Naming the owner is its own write, never a side effect of seeding: the migration creates
        // the server row with owner_id = MIN(users.id), which is NULL on a fresh database because no
        // account exists at migration time, and EnsureSeededAsync is keyed on that row and so
        // no-ops. This is the write "server set-owner" performs.
        var server = await db.Server.FirstAsync(row => row.Id == Data.Server.RowId);
        server.OwnerId = owner.Id;

        // What registering an account does in the same transaction as its row: a cursor in every
        // text channel, so the owner starts read rather than owed every message.
        await Seed.EnsureReadRowsAsync(db, owner.Id, CancellationToken.None);
        await db.SaveChangesAsync();
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
