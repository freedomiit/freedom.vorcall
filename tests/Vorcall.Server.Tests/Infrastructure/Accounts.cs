using System.Net;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.DependencyInjection;
using Vorcall.Server.Auth;
using Vorcall.Server.Data;
using Vorcall.Server.Protocol;
using Xunit;

namespace Vorcall.Server.Tests.Infrastructure;

// A registered account and the tokens it currently holds.
internal sealed class Account(long userId, string username, string password, string access, string refresh)
{
    public long UserId { get; } = userId;

    public string Username { get; } = username;

    public string Password { get; set; } = password;

    public string Access { get; private set; } = access;

    public string Refresh { get; private set; } = refresh;

    public static Account From(TokenResponse tokens, string password)
        => new(tokens.UserId, tokens.Username, password, tokens.AccessToken, tokens.RefreshToken);

    public TokenResponse Adopt(TokenResponse tokens)
    {
        Access = tokens.AccessToken;
        Refresh = tokens.RefreshToken;
        return tokens;
    }

    // Id and name only: an account's tokens never reach a test's output.
    public override string ToString() => $"{Username}(id={UserId})";
}

// The auth endpoints, plus the invite row a registration needs: the same row "invites new"
// writes, minted straight into the host's database.
internal static class Accounts
{
    public const string Password = "suite-password-1";
    public const string NewPassword = "suite-password-2";

    public static async Task<string> MintInviteAsync(VorcallFactory factory, int days = 7)
    {
        var code = Credentials.NewInviteCode();
        var now = DateTime.UtcNow;
        var contexts = factory.Services.GetRequiredService<IDbContextFactory<AppDbContext>>();
        await using var db = await contexts.CreateDbContextAsync();
        db.Invites.Add(new Invite
        {
            CodeHash = Credentials.Sha256Hex(code),
            CreatedAt = now,
            ExpiresAt = now.AddDays(days),
        });
        await db.SaveChangesAsync();
        return Credentials.FormatInviteCode(code);
    }

    // A fresh account under a unique name, registered with a fresh invite.
    public static async Task<Account> RegisterAsync(VorcallFactory factory, string prefix)
    {
        var invite = await MintInviteAsync(factory);
        var response = await RegisterRawAsync(factory, Names.Next(prefix), Password, invite);
        Assert.True(response.Status == HttpStatusCode.Created, $"register {prefix}: {(int)response.Status}");
        return Account.From(response.As(TokenResponse.Parser), Password);
    }

    public static Task<ProtoResponse> RegisterRawAsync(VorcallFactory factory, string username, string password, string invite)
        => Proto.PostAsync(
            factory,
            "/api/auth/register",
            new RegisterRequest { Username = username, Password = password, InviteCode = invite });

    public static Task<ProtoResponse> LoginRawAsync(VorcallFactory factory, string username, string password)
        => Proto.PostAsync(factory, "/api/auth/login", new LoginRequest { Username = username, Password = password });

    // Signs the account in again and adopts the new pair.
    public static async Task<TokenResponse> LoginAsync(VorcallFactory factory, Account account)
    {
        var response = await LoginRawAsync(factory, account.Username, account.Password);
        Assert.True(response.Status == HttpStatusCode.OK, $"login {account}: {(int)response.Status}");
        return account.Adopt(response.As(TokenResponse.Parser));
    }

    public static Task<ProtoResponse> RefreshRawAsync(VorcallFactory factory, string refreshToken)
        => Proto.PostAsync(factory, "/api/auth/refresh", new RefreshRequest { RefreshToken = refreshToken });

    public static async Task<TokenResponse> RefreshAsync(VorcallFactory factory, Account account)
    {
        var response = await RefreshRawAsync(factory, account.Refresh);
        Assert.True(response.Status == HttpStatusCode.OK, $"refresh {account}: {(int)response.Status}");
        return account.Adopt(response.As(TokenResponse.Parser));
    }

    public static Task<ProtoResponse> LogoutAsync(VorcallFactory factory, string refreshToken)
        => Proto.PostAsync(factory, "/api/auth/logout", new LogoutRequest { RefreshToken = refreshToken });

    public static Task<ProtoResponse> ChangePasswordAsync(
        VorcallFactory factory,
        string bearer,
        string current,
        string next,
        string refreshToken)
        => Proto.PostAsync(
            factory,
            "/api/auth/password",
            new ChangePasswordRequest { CurrentPassword = current, NewPassword = next, RefreshToken = refreshToken },
            bearer);

    public static async Task<UserList> ListUsersAsync(VorcallFactory factory, string bearer)
    {
        var response = await Proto.GetAsync(factory, "/api/users", bearer);
        Assert.True(response.Status == HttpStatusCode.OK, $"users: {(int)response.Status}");
        return response.As(UserList.Parser);
    }
}
