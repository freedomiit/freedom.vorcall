using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Hosting;
using Vorcall.Server.Auth;
using Vorcall.Server.Data;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class SweeperTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Sweep_deletes_only_rows_seven_days_past_expiry_or_revocation()
    {
        var dave = await Accounts.RegisterAsync(Server, "sweepe");
        var now = DateTime.UtcNow;
        var contexts = Server.Services.GetRequiredService<IDbContextFactory<AppDbContext>>();

        long live, expiredLongAgo, revokedLongAgo, expiredRecently, revokedRecently;
        await using (var db = await contexts.CreateDbContextAsync())
        {
            live = await db.RefreshTokens.Where(t => t.UserId == dave.UserId).Select(t => t.Id).SingleAsync();

            var rows = new[]
            {
                Row(dave.UserId, now, expiresAt: now.AddDays(-8), revokedAt: null),
                Row(dave.UserId, now, expiresAt: now.AddDays(30), revokedAt: now.AddDays(-8)),
                Row(dave.UserId, now, expiresAt: now.AddDays(-6), revokedAt: null),
                Row(dave.UserId, now, expiresAt: now.AddDays(30), revokedAt: now.AddDays(-6)),
            };
            db.RefreshTokens.AddRange(rows);
            await db.SaveChangesAsync();
            expiredLongAgo = rows[0].Id;
            revokedLongAgo = rows[1].Id;
            expiredRecently = rows[2].Id;
            revokedRecently = rows[3].Id;
        }

        var sweeper = Server.Services.GetServices<IHostedService>().OfType<RefreshTokenSweeper>().Single();
        var removed = await sweeper.SweepAsync(now);
        Assert.True(removed >= 2, $"swept {removed} rows");

        await using (var db = await contexts.CreateDbContextAsync())
        {
            var remaining = await db.RefreshTokens
                .Where(t => t.UserId == dave.UserId)
                .Select(t => t.Id)
                .ToListAsync();
            Assert.DoesNotContain(expiredLongAgo, remaining);
            Assert.DoesNotContain(revokedLongAgo, remaining);
            Assert.Contains(expiredRecently, remaining);
            Assert.Contains(revokedRecently, remaining);
            Assert.Contains(live, remaining);
        }

        await Accounts.RefreshAsync(Server, dave);
    }

    [Fact]
    public void Grace_is_seven_days()
    {
        Assert.Equal(TimeSpan.FromDays(7), RefreshTokenSweeper.Grace);
    }

    private static RefreshToken Row(long userId, DateTime createdAt, DateTime expiresAt, DateTime? revokedAt) => new()
    {
        UserId = userId,
        TokenHash = Credentials.Sha256Hex(Guid.NewGuid().ToString("N")),
        FamilyId = Guid.NewGuid(),
        CreatedAt = createdAt,
        LastUsedAt = createdAt,
        ExpiresAt = expiresAt,
        RevokedAt = revokedAt,
    };
}
