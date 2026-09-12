using System.Globalization;
using System.Net;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class LoginLockoutTests(ServerFixture fixture)
{
    private const string LockedDetail = @"^too many attempts, try again in \d+s$";

    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Login_refuses_a_wrong_password_and_reads_the_username_case_insensitively()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");

        var wrong = await Accounts.LoginRawAsync(Server, alice.Username, "wrong-password");
        Assert.Equal(HttpStatusCode.Unauthorized, wrong.Status);
        Assert.Equal("invalid username or password", wrong.Detail);

        var upper = await Accounts.LoginRawAsync(Server, alice.Username.ToUpperInvariant(), Accounts.Password);
        Assert.Equal(HttpStatusCode.OK, upper.Status);
        var tokens = upper.As(TokenResponse.Parser);
        Assert.Equal(alice.UserId, tokens.UserId);
        Assert.Equal(alice.Username, tokens.Username);
    }

    [Fact]
    public async Task Five_failures_lock_the_account_and_the_right_password_is_locked_out_too()
    {
        var bob = await Accounts.RegisterAsync(Server, "bob");
        for (var attempt = 0; attempt < 5; attempt++)
        {
            var failed = await Accounts.LoginRawAsync(Server, bob.Username, "wrong-password");
            Assert.Equal(HttpStatusCode.Unauthorized, failed.Status);
            Assert.Equal("invalid username or password", failed.Detail);
        }

        var locked = await Accounts.LoginRawAsync(Server, bob.Username, "wrong-password");
        Assert.Equal(HttpStatusCode.TooManyRequests, locked.Status);
        var retryAfter = int.Parse(locked.Header("Retry-After") ?? "0", CultureInfo.InvariantCulture);
        Assert.InRange(retryAfter, 1, 60);
        Assert.Matches(LockedDetail, locked.Detail);

        var rightPassword = await Accounts.LoginRawAsync(Server, bob.Username.ToUpperInvariant(), Accounts.Password);
        Assert.Equal(HttpStatusCode.TooManyRequests, rightPassword.Status);
        Assert.Matches(LockedDetail, rightPassword.Detail);
    }
}
