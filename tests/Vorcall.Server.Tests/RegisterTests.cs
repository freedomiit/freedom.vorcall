using System.Net;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class RegisterTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Register_refuses_bad_formats_and_unusable_invites_without_spending_the_invite()
    {
        var invite = await Accounts.MintInviteAsync(Server);
        var name = Names.Next("reject");

        var longName = await Accounts.RegisterRawAsync(Server, new string('n', 33), Accounts.Password, invite);
        Assert.Equal(HttpStatusCode.BadRequest, longName.Status);
        Assert.Equal("username must be 1..32 characters without control characters", longName.Detail);

        var shortPassword = await Accounts.RegisterRawAsync(Server, name, "short", invite);
        Assert.Equal(HttpStatusCode.BadRequest, shortPassword.Status);
        Assert.Equal("password must be 8..128 characters", shortPassword.Detail);

        var shortInvite = await Accounts.RegisterRawAsync(Server, name, Accounts.Password, "TOO-SHORT");
        Assert.Equal(HttpStatusCode.BadRequest, shortInvite.Status);
        Assert.Equal("invite code must be 20 characters", shortInvite.Detail);

        var unknownInvite = await Accounts.RegisterRawAsync(Server, name, Accounts.Password, "AAAAA-BBBBB-CCCCC-DDDDD");
        Assert.Equal(HttpStatusCode.Forbidden, unknownInvite.Status);
        Assert.Equal("invite code is invalid, used or expired", unknownInvite.Detail);

        var registered = await Accounts.RegisterRawAsync(Server, name, Accounts.Password, invite);
        Assert.Equal(HttpStatusCode.Created, registered.Status);
    }

    [Fact]
    public async Task Register_creates_the_account_and_answers_a_token_pair()
    {
        var invite = await Accounts.MintInviteAsync(Server);
        var name = Names.Next("alice");

        var response = await Accounts.RegisterRawAsync(Server, name, Accounts.Password, invite);
        Assert.Equal(HttpStatusCode.Created, response.Status);

        var tokens = response.As(TokenResponse.Parser);
        Assert.Equal(900u, tokens.ExpiresIn);
        Assert.Equal(name, tokens.Username);
        Assert.True(tokens.UserId > 0);
        Assert.NotEmpty(tokens.AccessToken);
        Assert.NotEmpty(tokens.RefreshToken);
    }

    [Fact]
    public async Task Register_keeps_the_invite_when_the_username_is_taken()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var invite = await Accounts.MintInviteAsync(Server);

        var taken = await Accounts.RegisterRawAsync(Server, alice.Username.ToUpperInvariant(), Accounts.Password, invite);
        Assert.Equal(HttpStatusCode.Conflict, taken.Status);
        Assert.Equal("username is taken", taken.Detail);

        var bob = await Accounts.RegisterRawAsync(Server, Names.Next("bob"), Accounts.Password, invite);
        Assert.Equal(HttpStatusCode.Created, bob.Status);
    }

    [Fact]
    public async Task Register_refuses_an_invite_that_was_already_used()
    {
        var invite = await Accounts.MintInviteAsync(Server);
        var first = await Accounts.RegisterRawAsync(Server, Names.Next("alice"), Accounts.Password, invite);
        Assert.Equal(HttpStatusCode.Created, first.Status);

        var second = await Accounts.RegisterRawAsync(Server, Names.Next("dave"), Accounts.Password, invite);
        Assert.Equal(HttpStatusCode.Forbidden, second.Status);
        Assert.Equal("invite code is invalid, used or expired", second.Detail);
    }
}
