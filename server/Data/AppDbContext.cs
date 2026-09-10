using Microsoft.EntityFrameworkCore;

namespace Vorcall.Server.Data;

public class AppDbContext(DbContextOptions<AppDbContext> options) : DbContext(options)
{
    public DbSet<Message> Messages => Set<Message>();

    protected override void OnModelCreating(ModelBuilder modelBuilder)
    {
        var message = modelBuilder.Entity<Message>();
        message.ToTable("messages");
        message.HasKey(m => m.Id);
        message.Property(m => m.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        message.Property(m => m.Author).HasColumnName("author").HasMaxLength(32).IsRequired();
        message.Property(m => m.Text).HasColumnName("text").HasMaxLength(2000).IsRequired();
        message.Property(m => m.SentAt).HasColumnName("sent_at").HasColumnType("timestamp with time zone").IsRequired();
    }
}
