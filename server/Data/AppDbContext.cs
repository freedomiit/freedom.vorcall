using Microsoft.EntityFrameworkCore;

namespace Vorcall.Server.Data;

public class AppDbContext(DbContextOptions<AppDbContext> options) : DbContext(options)
{
    public DbSet<Message> Messages => Set<Message>();

    public DbSet<User> Users => Set<User>();

    public DbSet<RefreshToken> RefreshTokens => Set<RefreshToken>();

    public DbSet<Invite> Invites => Set<Invite>();

    protected override void OnModelCreating(ModelBuilder modelBuilder)
    {
        var message = modelBuilder.Entity<Message>();
        message.ToTable("messages");
        message.HasKey(m => m.Id);
        message.Property(m => m.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        message.Property(m => m.Author).HasColumnName("author").HasMaxLength(32).IsRequired();
        message.Property(m => m.Text).HasColumnName("text").HasMaxLength(2000).IsRequired();
        message.Property(m => m.SentAt).HasColumnName("sent_at").HasColumnType("timestamp with time zone").IsRequired();

        // The default backfills every message written before rooms existed into general, so the
        // column can be required without rewriting a single existing row.
        message.Property(m => m.RoomId).HasColumnName("room_id").HasMaxLength(32).IsRequired().HasDefaultValue("general");
        message.Property(m => m.UserId).HasColumnName("user_id");
        message.HasIndex(m => new { m.RoomId, m.Id });

        // No navigation: a message keeps its author text forever, and deleting the account only
        // detaches the id.
        message.HasOne<User>().WithMany().HasForeignKey(m => m.UserId).OnDelete(DeleteBehavior.SetNull);

        var user = modelBuilder.Entity<User>();
        user.ToTable("users");
        user.HasKey(u => u.Id);
        user.Property(u => u.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        user.Property(u => u.Username).HasColumnName("username").HasMaxLength(32).IsRequired();
        user.Property(u => u.UsernameNormalized).HasColumnName("username_normalized").HasMaxLength(32).IsRequired();
        user.Property(u => u.PasswordHash).HasColumnName("password_hash").HasColumnType("text").IsRequired();
        user.Property(u => u.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();
        user.Property(u => u.LastClientVersion).HasColumnName("last_client_version").HasMaxLength(64);
        user.Property(u => u.LastClientPlatform).HasColumnName("last_client_platform").HasMaxLength(64);
        user.Property(u => u.LastSeenAt).HasColumnName("last_seen_at").HasColumnType("timestamp with time zone");
        user.HasIndex(u => u.UsernameNormalized).IsUnique();

        var refreshToken = modelBuilder.Entity<RefreshToken>();
        refreshToken.ToTable("refresh_tokens");
        refreshToken.HasKey(t => t.Id);
        refreshToken.Property(t => t.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        refreshToken.Property(t => t.UserId).HasColumnName("user_id").IsRequired();
        refreshToken.Property(t => t.TokenHash).HasColumnName("token_hash").HasColumnType("character(64)").IsRequired();
        refreshToken.Property(t => t.FamilyId).HasColumnName("family_id").IsRequired();
        refreshToken.Property(t => t.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();
        refreshToken.Property(t => t.LastUsedAt).HasColumnName("last_used_at").HasColumnType("timestamp with time zone").IsRequired();
        refreshToken.Property(t => t.ExpiresAt).HasColumnName("expires_at").HasColumnType("timestamp with time zone").IsRequired();
        refreshToken.Property(t => t.RevokedAt).HasColumnName("revoked_at").HasColumnType("timestamp with time zone");
        refreshToken.Property(t => t.ReplacedById).HasColumnName("replaced_by_id");
        refreshToken.HasIndex(t => t.TokenHash).IsUnique();
        refreshToken.HasIndex(t => t.UserId);
        refreshToken.HasIndex(t => t.FamilyId);
        refreshToken.HasOne(t => t.User).WithMany().HasForeignKey(t => t.UserId).OnDelete(DeleteBehavior.Cascade);

        var invite = modelBuilder.Entity<Invite>();
        invite.ToTable("invites");
        invite.HasKey(i => i.Id);
        invite.Property(i => i.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        invite.Property(i => i.CodeHash).HasColumnName("code_hash").HasColumnType("character(64)").IsRequired();
        invite.Property(i => i.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();
        invite.Property(i => i.ExpiresAt).HasColumnName("expires_at").HasColumnType("timestamp with time zone").IsRequired();
        invite.Property(i => i.UsedAt).HasColumnName("used_at").HasColumnType("timestamp with time zone");
        invite.Property(i => i.UsedByUserId).HasColumnName("used_by_user_id");
        invite.HasIndex(i => i.CodeHash).IsUnique();
    }
}
