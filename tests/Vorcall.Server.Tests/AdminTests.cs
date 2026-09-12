using System.Net;
using System.Net.Http.Json;
using System.Text.Json;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.DependencyInjection;
using Vorcall.Server.Api;
using Vorcall.Server.Auth;
using Vorcall.Server.Cli;
using Vorcall.Server.Data;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

// The admin endpoints are the CLI's only reach into the live server; the CLI's database half of an
// account lock, its lift and an invite revocation is replayed here through the host's own services.
[Collection(ServerCollection.Name)]
public sealed class AdminTests(ServerFixture fixture)
{
    private const int Kicked = 4001;

    // The owner's account lock ("users disable"), not the in-app moderation ban.
    private const int Disabled = 4003;

    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Kick_closes_the_live_socket_with_4001_and_the_account_can_come_back()
    {
        var alice = await Accounts.RegisterAsync(Server, "kicka");
        await using var ws = await WsClient.ConnectAsync(Server, alice);

        var kick = await KickAsync(Server, alice.UserId, Kicked, reason: null);
        Assert.Equal(HttpStatusCode.OK, kick.Status);
        Assert.True(Closed(kick));

        await ws.ExpectClosedAsync(Kicked);

        await using var back = await WsClient.ConnectAsync(Server, alice, "kicka-back");
    }

    [Fact]
    public async Task Kick_of_an_offline_account_answers_closed_false()
    {
        var bob = await Accounts.RegisterAsync(Server, "kickb");

        var kick = await KickAsync(Server, bob.UserId, Kicked, reason: null);
        Assert.Equal(HttpStatusCode.OK, kick.Status);
        Assert.False(Closed(kick));
    }

    [Fact]
    public async Task Admin_endpoints_answer_404_without_the_key_and_with_a_wrong_key()
    {
        var kickBody = new { userId = 1L, code = Kicked, reason = (string?)null };
        var refreshBody = new { userId = 1L };

        foreach (var adminKey in new string?[] { null, "wrong-" + ServerFixture.AdminKey })
        {
            var kick = await AdminPostAsync(Server, "/api/admin/kick", kickBody, adminKey);
            Assert.Equal(HttpStatusCode.NotFound, kick.Status);

            var refresh = await AdminPostAsync(Server, "/api/admin/refresh-account", refreshBody, adminKey);
            Assert.Equal(HttpStatusCode.NotFound, refresh.Status);
        }

        var badCode = await KickAsync(Server, 1, 4000, reason: null);
        Assert.Equal(HttpStatusCode.BadRequest, badCode.Status);
        Assert.Equal("code must be 4001 or 4003", JsonDetail(badCode));

        var noUser = await KickAsync(Server, 0, Kicked, reason: null);
        Assert.Equal(HttpStatusCode.BadRequest, noUser.Status);
        Assert.Equal("userId is required", JsonDetail(noUser));

        var noUserRefresh = await AdminPostAsync(Server, "/api/admin/refresh-account", new { userId = 0L }, ServerFixture.AdminKey);
        Assert.Equal(HttpStatusCode.BadRequest, noUserRefresh.Status);
        Assert.Equal("userId is required", JsonDetail(noUserRefresh));
    }

    [Fact]
    public async Task Disable_closes_with_4003_and_refuses_login_refresh_and_upgrade_until_enabled()
    {
        var carol = await Accounts.RegisterAsync(Server, "lockc");
        await using var ws = await WsClient.ConnectAsync(Server, carol);
        var access = carol.Access;
        var refresh = carol.Refresh;

        // What "users disable" writes before it talks to the server.
        var now = DateTime.UtcNow;
        await SetDisabledAtAsync(Server, carol.UserId, now);
        var tokens = Server.Services.GetRequiredService<TokenService>();
        await using (var db = await Contexts(Server).CreateDbContextAsync())
        {
            await tokens.RevokeAllAsync(db, carol.UserId, keepTokenId: null, now);
        }

        Assert.Equal(HttpStatusCode.NoContent, (await RefreshAccountAsync(Server, carol.UserId)).Status);
        var kick = await KickAsync(Server, carol.UserId, Disabled, reason: null);
        Assert.Equal(HttpStatusCode.OK, kick.Status);
        Assert.True(Closed(kick));
        await ws.ExpectClosedAsync(Disabled);

        var login = await Accounts.LoginRawAsync(Server, carol.Username, carol.Password);
        Assert.Equal(HttpStatusCode.Forbidden, login.Status);
        Assert.Equal("account disabled", login.Detail);

        // The account gate is read before the rotation, from the account the presented row names,
        // so a disabled account hears why rather than the 401 its revoked token alone would answer.
        var refreshed = await Accounts.RefreshRawAsync(Server, refresh);
        Assert.Equal(HttpStatusCode.Forbidden, refreshed.Status);
        Assert.Equal("account disabled", refreshed.Detail);

        var upgrade = await Assert.ThrowsAsync<InvalidOperationException>(() => WsClient.OpenAsync(Server, access, "lockc-disabled"));
        Assert.Contains("401", upgrade.Message);

        var rest = await Proto.GetAsync(Server, "/api/users", access);
        Assert.Equal(HttpStatusCode.Unauthorized, rest.Status);

        // What "users enable" does.
        await SetDisabledAtAsync(Server, carol.UserId, null);
        Assert.Equal(HttpStatusCode.NoContent, (await RefreshAccountAsync(Server, carol.UserId)).Status);

        await Accounts.LoginAsync(Server, carol);
        await using var back = await WsClient.ConnectAsync(Server, carol, "lockc-back");
    }

    // The CLI's live kick can fail (no admin key, wrong AdminUrl); the lock must still reach an
    // open socket on its own.
    [Fact]
    public async Task Disable_without_the_kick_closes_the_socket_at_its_next_write_frame()
    {
        var dave = await Accounts.RegisterAsync(Server, "lockd");
        await using var ws = await WsClient.ConnectAsync(Server, dave);

        var now = DateTime.UtcNow;
        await SetDisabledAtAsync(Server, dave.UserId, now);
        var tokens = Server.Services.GetRequiredService<TokenService>();
        await using (var db = await Contexts(Server).CreateDbContextAsync())
        {
            await tokens.RevokeAllAsync(db, dave.UserId, keepTokenId: null, now);
        }

        Assert.Equal(HttpStatusCode.NoContent, (await RefreshAccountAsync(Server, dave.UserId)).Status);

        await ws.SendAsync(Frames.Send("still here", ws.Session.GeneralId));
        await ws.ExpectClosedAsync(Disabled);

        await SetDisabledAtAsync(Server, dave.UserId, null);
        Assert.Equal(HttpStatusCode.NoContent, (await RefreshAccountAsync(Server, dave.UserId)).Status);
    }

    [Fact]
    public async Task Revoked_invite_cannot_register()
    {
        var invite = await Accounts.MintInviteAsync(Server);
        Assert.True(Credentials.TryNormalizeInviteCode(invite, out var code));
        var hash = Credentials.Sha256Hex(code);

        // What "invites revoke" writes.
        await using (var db = await Contexts(Server).CreateDbContextAsync())
        {
            var revoked = await db.Invites
                .Where(i => i.CodeHash == hash && i.UsedAt == null && i.RevokedAt == null)
                .ExecuteUpdateAsync(setters => setters.SetProperty(i => i.RevokedAt, (DateTime?)DateTime.UtcNow));
            Assert.Equal(1, revoked);
        }

        var refused = await Accounts.RegisterRawAsync(Server, Names.Next("revd"), Accounts.Password, invite);
        Assert.Equal(HttpStatusCode.Forbidden, refused.Status);
        Assert.Equal("invite code is invalid, used or expired", refused.Detail);

        var fresh = await Accounts.MintInviteAsync(Server);
        var registered = await Accounts.RegisterRawAsync(Server, Names.Next("revd"), Accounts.Password, fresh);
        Assert.Equal(HttpStatusCode.Created, registered.Status);
    }

    [Theory]
    [InlineData(true, "invites")]
    [InlineData(true, "users", "list")]
    [InlineData(true, "server", "show")]
    [InlineData(false)]
    [InlineData(false, "run")]
    public void IsCliInvocation_recognises_only_the_three_admin_verbs(bool expected, params string[] args)
    {
        Assert.Equal(expected, AdminCli.IsCliInvocation(args));
    }

    private static IDbContextFactory<AppDbContext> Contexts(VorcallFactory factory)
        => factory.Services.GetRequiredService<IDbContextFactory<AppDbContext>>();

    private static async Task SetDisabledAtAsync(VorcallFactory factory, long userId, DateTime? disabledAt)
    {
        await using var db = await Contexts(factory).CreateDbContextAsync();
        var user = await db.Users.SingleAsync(u => u.Id == userId);
        user.DisabledAt = disabledAt;
        await db.SaveChangesAsync();
    }

    private static Task<ProtoResponse> KickAsync(VorcallFactory factory, long userId, int code, string? reason)
        => AdminPostAsync(factory, "/api/admin/kick", new { userId, code, reason }, ServerFixture.AdminKey);

    private static Task<ProtoResponse> RefreshAccountAsync(VorcallFactory factory, long userId)
        => AdminPostAsync(factory, "/api/admin/refresh-account", new { userId }, ServerFixture.AdminKey);

    // The endpoints read JSON, not protobuf, and their gate is a header of their own on top of
    // the door key every /api request carries.
    private static Task<ProtoResponse> AdminPostAsync(VorcallFactory factory, string path, object body, string? adminKey)
        => Proto.SendAsync(
            factory,
            HttpMethod.Post,
            path,
            JsonContent.Create(body),
            bearer: null,
            key: ServerFixture.ServerKey,
            configure: request =>
            {
                if (adminKey is not null)
                {
                    request.Headers.Add(AdminEndpoints.AdminKeyHeader, adminKey);
                }
            });

    private static bool Closed(ProtoResponse response)
        => JsonDocument.Parse(response.Body).RootElement.GetProperty("closed").GetBoolean();

    private static string JsonDetail(ProtoResponse response)
        => JsonDocument.Parse(response.Body).RootElement.GetProperty("detail").GetString() ?? string.Empty;
}
