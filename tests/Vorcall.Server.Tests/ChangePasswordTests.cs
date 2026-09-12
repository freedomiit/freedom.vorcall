using System.Net;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class ChangePasswordTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task ChangePassword_refuses_a_wrong_current_password_and_a_short_new_one()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");

        var wrong = await Accounts.ChangePasswordAsync(Server, alice.Access, "wrong-password", Accounts.NewPassword, alice.Refresh);
        Assert.Equal(HttpStatusCode.Unauthorized, wrong.Status);
        Assert.Equal("current password is wrong", wrong.Detail);

        var tooShort = await Accounts.ChangePasswordAsync(Server, alice.Access, Accounts.Password, "short", alice.Refresh);
        Assert.Equal(HttpStatusCode.BadRequest, tooShort.Status);
        Assert.Equal("password must be 8..128 characters", tooShort.Detail);
    }

    [Fact]
    public async Task ChangePassword_revokes_every_other_session_and_keeps_the_presented_one()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var otherRefresh = alice.Refresh;
        await Accounts.LoginAsync(Server, alice);

        var changed = await Accounts.ChangePasswordAsync(Server, alice.Access, Accounts.Password, Accounts.NewPassword, alice.Refresh);
        Assert.Equal(HttpStatusCode.NoContent, changed.Status);
        alice.Password = Accounts.NewPassword;

        var revoked = await Accounts.RefreshRawAsync(Server, otherRefresh);
        Assert.Equal(HttpStatusCode.Unauthorized, revoked.Status);
        Assert.Equal("refresh token is invalid", revoked.Detail);

        await Accounts.RefreshAsync(Server, alice);

        var oldPassword = await Accounts.LoginRawAsync(Server, alice.Username, Accounts.Password);
        Assert.Equal(HttpStatusCode.Unauthorized, oldPassword.Status);
        Assert.Equal("invalid username or password", oldPassword.Detail);

        await Accounts.LoginAsync(Server, alice);
    }
}
