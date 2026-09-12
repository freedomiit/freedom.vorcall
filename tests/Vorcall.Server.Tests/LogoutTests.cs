using System.Net;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class LogoutTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Logout_kills_the_refresh_token_and_is_idempotent()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var refresh = alice.Refresh;

        var first = await Accounts.LogoutAsync(Server, refresh);
        Assert.Equal(HttpStatusCode.NoContent, first.Status);

        var dead = await Accounts.RefreshRawAsync(Server, refresh);
        Assert.Equal(HttpStatusCode.Unauthorized, dead.Status);
        Assert.Equal("refresh token is invalid", dead.Detail);

        var again = await Accounts.LogoutAsync(Server, refresh);
        Assert.Equal(HttpStatusCode.NoContent, again.Status);

        await Accounts.LoginAsync(Server, alice);
    }
}
