using System.Net.WebSockets;
using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Api;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Data;

var builder = WebApplication.CreateBuilder(args);

var connectionString = builder.Configuration.GetConnectionString("Default");
if (string.IsNullOrWhiteSpace(connectionString))
{
    throw new InvalidOperationException("Missing configuration 'ConnectionStrings:Default'.");
}

// The pre-shared key is the only door: refusing to boot without it is deliberate.
var serverKey = builder.Configuration["Vorcall:ServerKey"];
if (string.IsNullOrWhiteSpace(serverKey))
{
    throw new InvalidOperationException("Missing configuration 'Vorcall:ServerKey'.");
}

builder.Services.AddDbContextFactory<AppDbContext>(o => o.UseNpgsql(connectionString));
builder.Services.AddSingleton(new ServerKeyValidator(serverKey));
builder.Services.AddSingleton<ConnectionRegistry>();
builder.Services.AddSingleton<MessageService>();
builder.Services.AddSingleton<ChatSocketHandler>();

var app = builder.Build();

// Migrate before anything listens: a failure has to leave the port closed and let the
// container restart, rather than serve traffic against an unmigrated schema.
using (var scope = app.Services.CreateScope())
{
    using var db = scope.ServiceProvider.GetRequiredService<IDbContextFactory<AppDbContext>>().CreateDbContext();
    db.Database.Migrate();
}

app.UseWebSockets(new WebSocketOptions { KeepAliveInterval = TimeSpan.FromSeconds(30) });

// Segment matching, not equality: routing also serves "/ws/", which equality would leave ungated.
app.UseWhen(
    context => context.Request.Path.StartsWithSegments("/ws") || context.Request.Path.StartsWithSegments("/api"),
    keyed => keyed.UseMiddleware<ServerKeyMiddleware>());

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

app.MapGet("/ws", async (HttpContext context, ChatSocketHandler handler) =>
{
    if (!context.WebSockets.IsWebSocketRequest)
    {
        context.Response.StatusCode = StatusCodes.Status400BadRequest;
        return;
    }

    using var socket = await context.WebSockets.AcceptWebSocketAsync();
    await handler.HandleAsync(socket, context.RequestAborted);
});

app.MapGet("/api/messages", MessagesEndpoints.GetPageAsync);

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
