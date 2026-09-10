using System.Threading.RateLimiting;
using Google.Protobuf;
using Microsoft.AspNetCore.Authentication.JwtBearer;
using Microsoft.AspNetCore.Identity;
using Microsoft.AspNetCore.RateLimiting;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.Options;
using Microsoft.IdentityModel.Tokens;
using Vorcall.Server.Api;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Data;
using Vorcall.Server.Protocol;
using Vorcall.Server.Updates;
using Vorcall.Server.Voice;

namespace Vorcall.Server;

// Shared by the web path and the admin CLI: both need the configuration checks, the database
// and the account services. Only the web path adds the HTTP pipeline on top.
public static class ServiceSetup
{
    private const int PasswordHashIterations = 210_000;
    private const int AuthRequestsPerWindow = 10;

    public static void Configure(WebApplicationBuilder builder)
    {
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

        // Same reasoning for the signing key: without it every access token would be forgeable.
        var jwt = JwtOptions.FromConfiguration(builder.Configuration);

        // Unusable voice settings fail the boot the same way; an absent key is a valid default.
        var voice = VoiceOptions.FromConfiguration(builder.Configuration);

        var updates = UpdatesOptions.FromConfiguration(builder.Configuration);

        builder.Services.AddDbContextFactory<AppDbContext>(o => o.UseNpgsql(connectionString));
        builder.Services.AddSingleton(new ServerKeyValidator(serverKey));
        builder.Services.AddSingleton(jwt);
        builder.Services.AddSingleton(voice);
        builder.Services.AddSingleton(updates);
        builder.Services.AddSingleton<UpdateManifestStore>();

        // One instance in both roles: the registry signals over the very relay the host runs.
        builder.Services.AddSingleton<VoiceRelay>();
        builder.Services.AddHostedService(sp => sp.GetRequiredService<VoiceRelay>());
        builder.Services.AddSingleton<ConnectionRegistry>();
        builder.Services.AddSingleton<MessageService>();
        builder.Services.AddSingleton<ChatSocketHandler>();
        builder.Services.AddSingleton<IPasswordHasher<User>>(
            new PasswordHasher<User>(Options.Create(new PasswordHasherOptions { IterationCount = PasswordHashIterations })));
        builder.Services.AddSingleton<TokenService>();
        builder.Services.AddSingleton<LoginThrottle>();
        builder.Services.AddSingleton<AccountService>();

        builder.Services
            .AddAuthentication(JwtBearerDefaults.AuthenticationScheme)
            .AddJwtBearer(options =>
            {
                // Claims keep their wire names instead of being mapped to the legacy SOAP URIs.
                options.MapInboundClaims = false;
                options.TokenValidationParameters = new TokenValidationParameters
                {
                    ValidateIssuer = true,
                    ValidIssuer = JwtOptions.Issuer,
                    ValidateAudience = true,
                    ValidAudience = JwtOptions.Audience,
                    ValidateIssuerSigningKey = true,
                    IssuerSigningKey = jwt.Key,
                    ValidAlgorithms = [SecurityAlgorithms.HmacSha256],
                    ValidateLifetime = true,
                    ClockSkew = TimeSpan.FromSeconds(30),
                    NameClaimType = "name",
                };
            });
        builder.Services.AddAuthorization();

        builder.Services.AddRateLimiter(options =>
        {
            options.RejectionStatusCode = StatusCodes.Status429TooManyRequests;
            options.OnRejected = async (context, cancellationToken) =>
            {
                context.HttpContext.Response.StatusCode = StatusCodes.Status429TooManyRequests;
                context.HttpContext.Response.Headers.RetryAfter = "60";
                context.HttpContext.Response.ContentType = ProtobufBody.ContentType;
                await context.HttpContext.Response.Body.WriteAsync(
                    new ApiError { Detail = "too many requests" }.ToByteArray(),
                    cancellationToken);
            };

            // Partitioned by client IP, which UseForwardedHeaders has already resolved to the
            // address nginx saw.
            options.AddPolicy(AuthEndpoints.RateLimitPolicy, context =>
                RateLimitPartition.GetSlidingWindowLimiter(
                    context.Connection.RemoteIpAddress?.ToString() ?? "unknown",
                    _ => new SlidingWindowRateLimiterOptions
                    {
                        PermitLimit = AuthRequestsPerWindow,
                        Window = TimeSpan.FromMinutes(1),
                        SegmentsPerWindow = 6,
                        QueueLimit = 0,
                    }));
        });
    }
}
