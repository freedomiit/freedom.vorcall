using System.Globalization;
using System.Text;
using Microsoft.AspNetCore.Identity;
using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Data;
using Vorcall.Server.Updates;

namespace Vorcall.Server.Cli;

// The server image is also the admin tool, which is what makes
// "docker compose run --rm backend invites new" work without a second image.
public static class AdminCli
{
    private const int DefaultInviteDays = 7;
    private const int MaxVersionEcho = 32;
    private const int UsageExitCode = 2;

    public static bool IsCliInvocation(string[] args) => args.Length > 0 && args[0] is "invites" or "users" or "server";

    public static async Task<int> RunAsync(string[] args)
    {
        // Empty args on purpose: "--days 7" is a command flag, not a configuration override.
        var builder = WebApplication.CreateBuilder([]);
        builder.Logging.SetMinimumLevel(LogLevel.Warning);
        ServiceSetup.Configure(builder);

        await using var app = builder.Build();
        await using var db = await app.Services.GetRequiredService<IDbContextFactory<AppDbContext>>().CreateDbContextAsync();
        await db.Database.MigrateAsync();

        var now = DateTime.UtcNow;
        return args[0] switch
        {
            "invites" => await RunInvitesAsync(app.Services, db, args, now),
            "users" => await RunUsersAsync(app.Services, db, args, now),
            "server" => await RunServerAsync(app.Services, db, args, now),
            _ => Usage(),
        };
    }

    private static async Task<int> RunInvitesAsync(IServiceProvider services, AppDbContext db, string[] args, DateTime now)
    {
        switch (Subcommand(args))
        {
            case "new":
            {
                if (!TryParseDays(args, out var days))
                {
                    return Usage();
                }

                var invites = services.GetRequiredService<InviteService>();
                var (_, code, expiresAt) = await invites.CreateAsync(days, createdBy: null, now, CancellationToken.None);

                // Only the hash is stored, so this is the one and only time the code is legible.
                Console.WriteLine($"Invite code: {Credentials.FormatInviteCode(code)}");
                Console.WriteLine($"Expires: {Format(expiresAt)}");
                return 0;
            }

            case "list":
            {
                var invites = await db.Invites
                    .AsNoTracking()
                    .OrderBy(i => i.Id)
                    .Select(i => new
                    {
                        i.Id,
                        i.CreatedAt,
                        i.ExpiresAt,
                        i.UsedAt,
                        Username = db.Users.Where(u => u.Id == i.UsedByUserId).Select(u => u.Username).FirstOrDefault(),
                    })
                    .ToListAsync();

                foreach (var invite in invites)
                {
                    var state = invite.UsedAt is not null
                        ? $"used by {invite.Username ?? "(deleted account)"}"
                        : invite.ExpiresAt <= now ? "expired" : "unused";
                    Console.WriteLine($"{invite.Id}  created {Format(invite.CreatedAt)}  expires {Format(invite.ExpiresAt)}  {state}");
                }

                return 0;
            }

            default:
                return Usage();
        }
    }

    private static async Task<int> RunUsersAsync(IServiceProvider services, AppDbContext db, string[] args, DateTime now)
    {
        var tokens = services.GetRequiredService<TokenService>();
        switch (Subcommand(args))
        {
            case "list":
            {
                var users = await db.Users
                    .AsNoTracking()
                    .OrderBy(u => u.UsernameNormalized)
                    .Select(u => new
                    {
                        u.Id,
                        u.Username,
                        u.CreatedAt,
                        u.LastClientVersion,
                        u.LastClientPlatform,
                        u.LastSeenAt,
                        Sessions = db.RefreshTokens.Count(t => t.UserId == u.Id && t.RevokedAt == null && t.ExpiresAt > now),
                    })
                    .ToListAsync();

                foreach (var user in users)
                {
                    Console.WriteLine(
                        $"{user.Id}  {user.Username}  created {Format(user.CreatedAt)}  {user.Sessions} active session(s)"
                        + FormatClient(user.LastClientVersion, user.LastClientPlatform, user.LastSeenAt));
                }

                return 0;
            }

            case "outdated":
            {
                if (!TryParseMinVersion(args, out var requested))
                {
                    return Usage();
                }

                ClientVersion floor;
                if (requested is { } explicitFloor)
                {
                    floor = explicitFloor;
                }
                else
                {
                    var manifests = services.GetRequiredService<UpdateManifestStore>();
                    if (manifests.TryLoad() is not { } loaded)
                    {
                        Console.Error.WriteLine($"no manifest in {manifests.ReleasesDir}; pass --min X.Y.Z");
                        return 1;
                    }

                    // A published manifest whose version this server cannot compare is a release
                    // that needs fixing, not a missing one: say which it is.
                    if (!ClientVersion.TryParse(loaded.Manifest.Version, out var published))
                    {
                        Console.Error.WriteLine(
                            $"manifest in {manifests.ReleasesDir} has version \"{Truncate(loaded.Manifest.Version)}\" "
                            + "which is not MAJOR.MINOR.PATCH; pass --min X.Y.Z");
                        return 1;
                    }

                    floor = published;
                }

                var accounts = await db.Users
                    .AsNoTracking()
                    .OrderBy(u => u.UsernameNormalized)
                    .Select(u => new
                    {
                        u.Id,
                        u.Username,
                        u.LastClientVersion,
                        u.LastClientPlatform,
                        u.LastSeenAt,
                    })
                    .ToListAsync();

                foreach (var account in accounts)
                {
                    // A version that does not parse counts as outdated: the floor is the only
                    // build this server can vouch for.
                    if (ClientVersion.TryParse(account.LastClientVersion, out var version) && version.CompareTo(floor) >= 0)
                    {
                        continue;
                    }

                    Console.WriteLine(
                        $"{account.Id}  {account.Username}"
                        + FormatClient(account.LastClientVersion, account.LastClientPlatform, account.LastSeenAt));
                }

                return 0;
            }

            case "revoke-sessions":
            {
                if (Argument(args, 2) is not { } requested || string.IsNullOrWhiteSpace(requested))
                {
                    return Usage();
                }

                var user = await FindUserAsync(db, requested);
                if (user is null)
                {
                    return NoSuchUser();
                }

                await tokens.RevokeAllAsync(db, user.Id, keepTokenId: null, now);
                Console.WriteLine($"Revoked every refresh token of {user.Username}");
                return 0;
            }

            case "set-password":
            {
                if (Argument(args, 2) is not { } requested || string.IsNullOrWhiteSpace(requested))
                {
                    return Usage();
                }

                var user = await FindUserAsync(db, requested);
                if (user is null)
                {
                    return NoSuchUser();
                }

                var password = ReadPassword();
                if (!Credentials.IsValidPassword(password))
                {
                    Console.Error.WriteLine("password must be 8..128 characters");
                    return 1;
                }

                user.PasswordHash = services.GetRequiredService<IPasswordHasher<User>>().HashPassword(user, password);
                await db.SaveChangesAsync();

                // The old password may be the reason for the reset: every session it opened goes.
                await tokens.RevokeAllAsync(db, user.Id, keepTokenId: null, now);
                Console.WriteLine("Password updated");
                return 0;
            }

            default:
                return Usage();
        }
    }

    private static async Task<int> RunServerAsync(IServiceProvider services, AppDbContext db, string[] args, DateTime now)
    {
        var directory = services.GetRequiredService<ServerDirectory>();
        switch (Subcommand(args))
        {
            case "set-owner":
            {
                if (Argument(args, 2) is not { } requested || string.IsNullOrWhiteSpace(requested))
                {
                    return Usage();
                }

                var user = await FindUserAsync(db, requested);
                if (user is null)
                {
                    return NoSuchUser();
                }

                if (!await directory.SetOwnerAsync(user.Id, CancellationToken.None))
                {
                    Console.Error.WriteLine($"{user.Username} is banned and cannot own the server.");
                    return 1;
                }

                Console.WriteLine($"Server owner set to {user.Username} (id {user.Id}).");
                return 0;
            }

            case "show":
            {
                var server = await directory.LoadAsync(CancellationToken.None);
                var owner = server.OwnerId is { } ownerId
                    ? await db.Users.Where(u => u.Id == ownerId).Select(u => u.Username).FirstOrDefaultAsync() ?? "<none>"
                    : "<none>";

                Console.WriteLine($"Name: {server.Name}");
                Console.WriteLine($"Owner: {owner}");
                Console.WriteLine($"General channel: {server.GeneralChannelId?.ToString(CultureInfo.InvariantCulture) ?? "<none>"}");
                return 0;
            }

            default:
                return Usage();
        }
    }

    private static Task<User?> FindUserAsync(AppDbContext db, string? username)
        => Credentials.TryNormalizeUsername(username, out _, out var normalized)
            ? db.Users.FirstOrDefaultAsync(u => u.UsernameNormalized == normalized)
            : Task.FromResult<User?>(null);

    private static string ReadPassword()
    {
        Console.Write("New password: ");
        if (Console.IsInputRedirected)
        {
            return Console.ReadLine() ?? string.Empty;
        }

        // Echoing a password onto a shared terminal is exactly what this command exists to avoid.
        var typed = new StringBuilder();
        while (true)
        {
            var key = Console.ReadKey(intercept: true);
            switch (key.Key)
            {
                case ConsoleKey.Enter:
                    Console.WriteLine();
                    return typed.ToString();

                case ConsoleKey.Backspace:
                    if (typed.Length > 0)
                    {
                        typed.Length--;
                    }

                    break;

                default:
                    if (!char.IsControl(key.KeyChar))
                    {
                        typed.Append(key.KeyChar);
                    }

                    break;
            }
        }
    }

    // Exactly "users outdated" or "users outdated --min X.Y.Z", mirroring "invites new --days".
    // Null means the caller wants the floor taken from the published manifest.
    private static bool TryParseMinVersion(string[] args, out ClientVersion? floor)
    {
        floor = null;
        if (args.Length == 2)
        {
            return true;
        }

        if (args.Length != 4 || args[2] != "--min" || !ClientVersion.TryParse(args[3], out var requested))
        {
            return false;
        }

        floor = requested;
        return true;
    }

    private static bool TryParseDays(string[] args, out int days)
    {
        days = DefaultInviteDays;
        if (args.Length == 2)
        {
            return true;
        }

        return args.Length == 4
            && args[2] == "--days"
            && int.TryParse(args[3], NumberStyles.Integer, CultureInfo.InvariantCulture, out days)
            && InviteService.IsValidDays(days);
    }

    private static string Subcommand(string[] args) => Argument(args, 1) ?? string.Empty;

    private static string? Argument(string[] args, int index) => args.Length > index ? args[index] : null;

    private static string Truncate(string? value)
        => value is null ? string.Empty : value.Length <= MaxVersionEcho ? value : value[..MaxVersionEcho];

    private static string FormatClient(string? version, string? platform, DateTime? seenAt)
        => $"  client {version ?? "-"} {platform ?? "-"}  seen {(seenAt is null ? "-" : Format(seenAt.Value))}";

    private static string Format(DateTime value)
        => value.ToUniversalTime().ToString("yyyy-MM-dd'T'HH:mm:ss'Z'", CultureInfo.InvariantCulture);

    private static int NoSuchUser()
    {
        Console.Error.WriteLine("No such user.");
        return 1;
    }

    private static int Usage()
    {
        Console.Error.WriteLine(
            """
            Usage:
              invites new [--days N]           create an invite code (default 7 days, 1..365)
              invites list                     list every invite
              users list                       list every account
              users outdated [--min X.Y.Z]     list accounts below the published release
              users revoke-sessions <name>     revoke every refresh token of an account
              users set-password <name>        set an account's password and revoke its sessions
              server set-owner <username>      set who owns the server
              server show                      show the server's name, owner and general channel
            """);
        return UsageExitCode;
    }
}
