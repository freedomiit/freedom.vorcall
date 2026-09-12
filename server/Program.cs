using System.Net.WebSockets;
using Microsoft.AspNetCore.HttpOverrides;
using Microsoft.EntityFrameworkCore;
using Vorcall.Server;
using Vorcall.Server.Api;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Cli;
using Vorcall.Server.Data;

// The same binary is the admin tool: "dotnet Vorcall.Server.dll invites new" never listens.
if (AdminCli.IsCliInvocation(args))
{
    return await AdminCli.RunAsync(args);
}

var builder = WebApplication.CreateBuilder(args);
ServiceSetup.Configure(builder);

var app = builder.Build();

// Migrate before anything listens: a failure has to leave the port closed and let the
// container restart, rather than serve traffic against an unmigrated schema.
using (var scope = app.Services.CreateScope())
{
    using var db = scope.ServiceProvider.GetRequiredService<IDbContextFactory<AppDbContext>>().CreateDbContext();
    db.Database.Migrate();
}

// The server row, its @everyone role and its general channel are what everything else resolves
// against, so a database that has never been seeded is seeded here, before the mirror reads it.
await app.Services.GetRequiredService<ServerDirectory>().EnsureSeededAsync(CancellationToken.None);

// Presence and permissions are served from an in-memory mirror of the server tables, so it is
// filled from the migrated schema before the first connection can ask about a channel.
await app.Services.GetRequiredService<ConnectionRegistry>().LoadAsync(CancellationToken.None);

// nginx is the only thing that can reach this port: docker publishes it on host loopback. That
// is why the known-proxy list stays empty (which skips the check entirely) and why trusting the
// immediate hop is enough. ForwardLimit = 1 keeps only the entry nginx appends, so a client that
// sends its own X-Forwarded-For cannot forge the rate limiter's partition key.
var forwardedHeaders = new ForwardedHeadersOptions
{
    ForwardedHeaders = ForwardedHeaders.XForwardedFor | ForwardedHeaders.XForwardedProto,
    ForwardLimit = 1,
};
forwardedHeaders.KnownIPNetworks.Clear();
forwardedHeaders.KnownProxies.Clear();
app.UseForwardedHeaders(forwardedHeaders);

app.UseWebSockets(new WebSocketOptions { KeepAliveInterval = TimeSpan.FromSeconds(30) });

// Segment matching, not equality: routing also serves "/ws/", which equality would leave ungated.
app.UseWhen(
    context => context.Request.Path.StartsWithSegments("/ws") || context.Request.Path.StartsWithSegments("/api"),
    keyed => keyed.UseMiddleware<ServerKeyMiddleware>());

app.UseAuthentication();

// The upload policy partitions on the bearer's user id, so the principal has to exist first.
app.UseRateLimiter();
app.UseAuthorization();

app.MapGet("/health", async (IDbContextFactory<AppDbContext> contextFactory, ILogger<Program> logger) =>
{
    try
    {
        await using var db = await contextFactory.CreateDbContextAsync();
        await db.Database.ExecuteSqlRawAsync("SELECT 1");
        return Results.Json(new { status = "ok" });
    }
    catch (Exception ex)
    {
        logger.LogError(ex, "Health check failed");
        return Results.Json(new { status = "degraded" }, statusCode: StatusCodes.Status503ServiceUnavailable);
    }
});

app.MapGet("/ws", async (HttpContext context, ChatSocketHandler handler, MemberDirectory members) =>
{
    if (!context.WebSockets.IsWebSocketRequest)
    {
        context.Response.StatusCode = StatusCodes.Status400BadRequest;
        return;
    }

    // The token validated, so both claims are this server's own: a missing one is a bug, not a
    // caller error, and the socket is never accepted without an identity behind it.
    if (!BearerIdentity.TryGetUserId(context.User, out var userId))
    {
        context.Response.StatusCode = StatusCodes.Status401Unauthorized;
        return;
    }

    var username = BearerIdentity.GetUsername(context.User);
    if (username.Length == 0)
    {
        context.Response.StatusCode = StatusCodes.Status400BadRequest;
        return;
    }

    // An access token outlives the ban that revoked the account's refresh tokens, so the upgrade is
    // where a banned account is turned away. PROTOCOL.md § Moderation: a bare 403, which is how the
    // client tells this apart from the two gates above.
    if (await members.IsBannedAsync(userId, context.RequestAborted))
    {
        context.Response.StatusCode = StatusCodes.Status403Forbidden;
        return;
    }

    using var socket = await context.WebSockets.AcceptWebSocketAsync();
    await handler.HandleAsync(socket, userId, username, context.RequestAborted);
}).RequireAuthorization();

app.MapGet("/api/messages", MessagesEndpoints.GetPageAsync).RequireAuthorization();
UsersEndpoints.Map(app);
AuthEndpoints.Map(app);
UpdatesEndpoints.Map(app);
AttachmentsEndpoints.Map(app);
ImagesEndpoints.Map(app);
InvitesEndpoints.Map(app);
BansEndpoints.Map(app);

app.Lifetime.ApplicationStopping.Register(() =>
{
    var registry = app.Services.GetRequiredService<ConnectionRegistry>();
    var logger = app.Services.GetRequiredService<ILogger<Program>>();
    logger.LogInformation("Shutting down: closing {ConnectionCount} live connections", registry.Count);
    try
    {
        // Bounded on purpose: shutdown must not hang on a socket that never drains.
        if (!registry.CloseAllAsync(WebSocketCloseStatus.EndpointUnavailable, ChatSocketHandler.ShutdownReason).Wait(TimeSpan.FromSeconds(5)))
        {
            logger.LogWarning("Timed out closing {ConnectionCount} live connections during shutdown", registry.Count);
        }
    }
    catch (Exception ex)
    {
        logger.LogError(ex, "Failed to close live connections during shutdown");
    }
});

app.Run();
return 0;
