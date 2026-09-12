using System.Net;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class HttpRateLimitTests(ServerFixture fixture)
{
    [Fact]
    public async Task Auth_endpoints_answer_429_with_Retry_After_60_once_the_per_IP_window_is_spent()
    {
        var server = await fixture.TightAuthAsync();
        var limited = false;
        for (var attempt = 0; attempt < 15 && !limited; attempt++)
        {
            var response = await Accounts.LoginRawAsync(server, Names.Next("ghost"), "wrong-password");
            if (response.Status == HttpStatusCode.TooManyRequests)
            {
                Assert.Equal("too many requests", response.Detail);
                Assert.Equal("60", response.Header("Retry-After"));
                Assert.True(
                    attempt >= ServerFixture.TightAuthPerWindow,
                    $"limited on attempt {attempt + 1}, before the window of {ServerFixture.TightAuthPerWindow} was spent");
                limited = true;
            }
            else
            {
                Assert.Equal(HttpStatusCode.Unauthorized, response.Status);
            }
        }

        Assert.True(limited, "the per-IP limiter never fired within 15 attempts");
    }
}
