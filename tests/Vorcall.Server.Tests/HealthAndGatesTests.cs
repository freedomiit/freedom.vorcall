using System.Net;
using System.Net.Http.Headers;
using Vorcall.Server.Auth;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class HealthAndGatesTests(ServerFixture fixture)
{
    private static readonly string?[] BadKeys = [null, "wrong-key"];
    private static readonly string?[] BadBearers = [null, "not-a-jwt"];

    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Health_answers_ok_without_the_door_key()
    {
        using var response = await Server.Client.GetAsync("/health");
        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        Assert.Equal("{\"status\":\"ok\"}", await response.Content.ReadAsStringAsync());
    }

    [Fact]
    public async Task Rest_without_the_door_key_answers_401_with_the_key_scheme()
    {
        foreach (var key in BadKeys)
        {
            var response = await Proto.GetAsync(Server, "/api/messages", key: key);
            Assert.Equal(HttpStatusCode.Unauthorized, response.Status);
            Assert.Equal(ServerKeyMiddleware.HeaderName, response.Header("WWW-Authenticate"));
            Assert.Empty(response.Body);
        }
    }

    [Fact]
    public async Task WebSocket_upgrade_without_the_door_key_answers_401_with_the_key_scheme()
    {
        foreach (var key in BadKeys)
        {
            var response = await Proto.SendAsync(Server, HttpMethod.Get, "/ws", null, bearer: null, key: key, configure: Upgrade);
            Assert.Equal(HttpStatusCode.Unauthorized, response.Status);
            Assert.Equal(ServerKeyMiddleware.HeaderName, response.Header("WWW-Authenticate"));

            var refused = await Assert.ThrowsAsync<InvalidOperationException>(
                () => WsClient.OpenAsync(Server, bearer: null, label: "anon", key: key));
            Assert.Contains("401", refused.Message);
        }
    }

    [Fact]
    public async Task Rest_without_a_bearer_answers_401_with_the_bearer_scheme()
    {
        foreach (var path in new[] { "/api/messages", "/api/users" })
        {
            foreach (var bearer in BadBearers)
            {
                var response = await Proto.GetAsync(Server, path, bearer);
                Assert.Equal(HttpStatusCode.Unauthorized, response.Status);
                Assert.StartsWith("Bearer", response.Header("WWW-Authenticate") ?? string.Empty);
            }
        }

        var password = await Proto.PostAsync(Server, "/api/auth/password", new ChangePasswordRequest());
        Assert.Equal(HttpStatusCode.Unauthorized, password.Status);
        Assert.StartsWith("Bearer", password.Header("WWW-Authenticate") ?? string.Empty);
    }

    [Fact]
    public async Task WebSocket_upgrade_without_a_bearer_answers_401_with_the_bearer_scheme()
    {
        foreach (var bearer in BadBearers)
        {
            var response = await Proto.SendAsync(Server, HttpMethod.Get, "/ws", null, bearer, ServerFixture.ServerKey, Upgrade);
            Assert.Equal(HttpStatusCode.Unauthorized, response.Status);
            Assert.StartsWith("Bearer", response.Header("WWW-Authenticate") ?? string.Empty);

            var refused = await Assert.ThrowsAsync<InvalidOperationException>(() => WsClient.OpenAsync(Server, bearer, "anon"));
            Assert.Contains("401", refused.Message);
        }
    }

    [Fact]
    public async Task Rest_bodies_over_16_KiB_answer_413_and_garbage_answers_400()
    {
        var oversized = await Proto.PostBytesAsync(Server, "/api/auth/register", new byte[(16 * 1024) + 1], Proto.ContentType);
        Assert.Equal(HttpStatusCode.RequestEntityTooLarge, oversized.Status);

        var garbage = await Proto.PostBytesAsync(Server, "/api/auth/register", [0xFF, 0xFF, 0xFF, 0xFF], Proto.ContentType);
        Assert.Equal(HttpStatusCode.BadRequest, garbage.Status);
        Assert.Equal("malformed request body", garbage.Detail);
    }

    // The headers of a WebSocket upgrade, on a plain request: both gates answer before the
    // upgrade is even considered, so this is the same 401 a real client sees.
    private static void Upgrade(HttpRequestMessage request)
    {
        request.Headers.Connection.Add("Upgrade");
        request.Headers.Upgrade.Add(new ProductHeaderValue("websocket"));
        request.Headers.Add("Sec-WebSocket-Version", "13");
        request.Headers.Add("Sec-WebSocket-Key", Convert.ToBase64String(new byte[16]));
    }
}
