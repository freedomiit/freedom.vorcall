using System.Net;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class RefreshRotationTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Refresh_rotates_the_pair_and_a_replay_revokes_the_whole_family()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var oldAccess = alice.Access;
        var oldRefresh = alice.Refresh;

        var rotated = await Accounts.RefreshAsync(Server, alice);
        Assert.Equal(alice.UserId, rotated.UserId);
        Assert.Equal(900u, rotated.ExpiresIn);
        Assert.NotEqual(oldRefresh, rotated.RefreshToken);

        // Rotation is about refresh tokens: the old access token lives on until it expires.
        var listing = await Accounts.ListUsersAsync(Server, oldAccess);
        Assert.Contains(listing.Users, user => user.UserId == alice.UserId);

        var replay = await Accounts.RefreshRawAsync(Server, oldRefresh);
        Assert.Equal(HttpStatusCode.Unauthorized, replay.Status);
        Assert.Equal("refresh token is invalid", replay.Detail);

        var successor = await Accounts.RefreshRawAsync(Server, rotated.RefreshToken);
        Assert.Equal(HttpStatusCode.Unauthorized, successor.Status);
        Assert.Equal("refresh token is invalid", successor.Detail);

        await Accounts.LoginAsync(Server, alice);
    }
}
