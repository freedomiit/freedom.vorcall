using System.Globalization;
using System.Threading.RateLimiting;
using Google.Protobuf;
using Microsoft.AspNetCore.Authentication.JwtBearer;
using Microsoft.AspNetCore.Identity;
using Microsoft.AspNetCore.RateLimiting;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.Options;
using Microsoft.IdentityModel.Tokens;
using Vorcall.Server.Admin;
using Vorcall.Server.Api;
using Vorcall.Server.Attachments;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Data;
using Vorcall.Server.Diagnostics;
using Vorcall.Server.Metrics;
using Vorcall.Server.Protocol;
using Vorcall.Server.Streams;
using Vorcall.Server.Updates;
using Vorcall.Server.Voice;

namespace Vorcall.Server;

// Shared by the web path and the admin CLI: both need the configuration checks, the database
// and the account services. Only the web path adds the HTTP pipeline on top.
public static class ServiceSetup
{
    private const int PasswordHashIterations = 210_000;
    private const int DefaultAuthRequestsPerWindow = 10;
    private const int DefaultUploadRequestsPerWindow = 20;

    public static void Configure(WebApplicationBuilder builder)
    {
        var connectionString = builder.Configuration.GetConnectionString("Default");
        if (string.IsNullOrWhiteSpace(connectionString))
        {
            throw new InvalidOperationException("Missing configuration 'ConnectionStrings:Default'.");
        }

        // The pre-shared key is the only door. Unconfigured, one is generated into Vorcall:DataDir
        // and logged once, so a self-hosted server comes up on `docker compose up -d` with nothing
        // to fill in first; a deployment that sets the key never reaches the file.
        var serverKey = SecretStore.ServerKey(builder.Configuration);

        // Same for the signing key, except that it is never logged.
        var jwt = JwtOptions.FromConfiguration(builder.Configuration);

        // Unusable voice settings fail the boot the same way; an absent key is a valid default.
        var voice = VoiceOptions.FromConfiguration(builder.Configuration);

        var updates = UpdatesOptions.FromConfiguration(builder.Configuration);

        // Same again for the attachment quota: a typo must fail the boot, not silently fall
        // back to a default that fills the disk.
        var attachments = AttachmentsOptions.FromConfiguration(builder.Configuration);

        // The streamed-file proxy's switch and ceilings, on the same terms.
        var streams = StreamOptions.FromConfiguration(builder.Configuration);

        // Both rate-limit windows are knobs so a test host can turn them down to something it can
        // actually trip; a typo fails the boot like every other Vorcall setting rather than
        // silently widening a limit.
        var authRequestsPerWindow = ReadPositiveInt(
            builder.Configuration, "Vorcall:AuthRequestsPerWindow", DefaultAuthRequestsPerWindow);
        var uploadRequestsPerWindow = ReadPositiveInt(
            builder.Configuration, "Vorcall:UploadRequestsPerWindow", DefaultUploadRequestsPerWindow);

        builder.Services.AddDbContextFactory<AppDbContext>(o => o.UseNpgsql(connectionString));
        builder.Services.AddSingleton(new ServerKeyValidator(serverKey));
        builder.Services.AddSingleton(jwt);
        builder.Services.AddSingleton(voice);
        builder.Services.AddSingleton(updates);
        builder.Services.AddSingleton(attachments);
        builder.Services.AddSingleton(streams);
        builder.Services.AddSingleton<ServerMetrics>();
        builder.Services.AddSingleton<UpdateManifestStore>();
        builder.Services.AddSingleton<AttachmentStore>();
        builder.Services.AddSingleton<ImageStore>();
        builder.Services.AddSingleton<SoundStore>();
        builder.Services.AddHostedService<AttachmentSweeper>();

        // The proxy's rendezvous is in memory and the registry tells it when an owner goes; the
        // rows behind it and their sweep are the directory's.
        builder.Services.AddSingleton<StreamRegistry>();
        builder.Services.AddSingleton<StreamDirectory>();
        builder.Services.AddHostedService<StreamSweeper>();
        DiagnosticsSetup.Configure(builder);
        AdminSetup.Configure(builder);

        // One instance in both roles: the registry signals over the very relay the host runs.
        builder.Services.AddSingleton<VoiceRelay>();
        builder.Services.AddHostedService(sp => sp.GetRequiredService<VoiceRelay>());
        builder.Services.AddSingleton<ConnectionRegistry>();

        // The registry's mirror is built from these four and written back through them.
        builder.Services.AddSingleton<ServerDirectory>();
        builder.Services.AddSingleton<ChannelDirectory>();
        builder.Services.AddSingleton<RoleDirectory>();
        builder.Services.AddSingleton<MemberDirectory>();
        builder.Services.AddSingleton<MessageService>();
        builder.Services.AddSingleton<ChatSocketHandler>();
        builder.Services.AddSingleton<IPasswordHasher<User>>(
            new PasswordHasher<User>(Options.Create(new PasswordHasherOptions { IterationCount = PasswordHashIterations })));
        builder.Services.AddSingleton<TokenService>();
        builder.Services.AddSingleton<LoginThrottle>();
        builder.Services.AddSingleton<AccountService>();
        builder.Services.AddSingleton<InviteService>();

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

                options.Events = new JwtBearerEvents
                {
                    // A lock has to reach the tokens already minted, and this is the one place
                    // every bearer goes through: the REST endpoints and the /ws upgrade alike.
                    OnTokenValidated = async context =>
                    {
                        if (context.Principal is not { } principal || !BearerIdentity.TryGetUserId(principal, out var userId))
                        {
                            return;
                        }

                        var accounts = context.HttpContext.RequestServices.GetRequiredService<DisabledAccounts>();
                        if (await accounts.IsDisabledAsync(userId, context.HttpContext.RequestAborted))
                        {
                            context.Fail("account disabled");
                        }
                    },
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
                context.HttpContext.RequestServices.GetRequiredService<ServerMetrics>().CountHttpRateLimited();
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
                        PermitLimit = authRequestsPerWindow,
                        Window = TimeSpan.FromMinutes(1),
                        SegmentsPerWindow = 6,
                        QueueLimit = 0,
                    }));

            // Partitioned by the bearer's user id, per PROTOCOL.md.
            options.AddPolicy(AttachmentsEndpoints.RateLimitPolicy, context =>
                RateLimitPartition.GetSlidingWindowLimiter(
                    BearerIdentity.TryGetUserId(context.User, out var userId)
                        ? $"user:{userId}"
                        : $"ip:{context.Connection.RemoteIpAddress?.ToString() ?? "unknown"}",
                    _ => new SlidingWindowRateLimiterOptions
                    {
                        PermitLimit = uploadRequestsPerWindow,
                        Window = TimeSpan.FromMinutes(1),
                        SegmentsPerWindow = 6,
                        QueueLimit = 0,
                    }));
        });
    }

    // Absent is the default; present and unusable fails the boot, the same way an unusable
    // attachment quota does.
    private static int ReadPositiveInt(IConfiguration configuration, string key, int fallback)
    {
        var configured = configuration[key];
        if (string.IsNullOrWhiteSpace(configured))
        {
            return fallback;
        }

        if (!int.TryParse(configured.Trim(), NumberStyles.Integer, CultureInfo.InvariantCulture, out var value) || value < 1)
        {
            throw new InvalidOperationException($"Invalid configuration '{key}' (expected a whole number of at least 1).");
        }

        return value;
    }
}
