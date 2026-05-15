package jakarta.servlet.annotation;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * STUB of {@code jakarta.servlet.annotation.HandlesTypes}.
 *
 * <p>Marks the set of classes a {@link jakarta.servlet.ServletContainerInitializer}
 * is interested in. The container scans the webapp's classes for types that
 * extend, implement, or are annotated with any of the listed classes; the
 * resulting set is passed to {@code SCI.onStartup(...)} as its first
 * argument.
 *
 * <p>{@code @HandlesTypes} retention is {@code RUNTIME} so the container can
 * read it reflectively at deploy time; the target is {@code TYPE} since it
 * is applied to the SCI implementation class itself.
 *
 * <p>See {@code jakarta.servlet.ServletRequest} for the broader stub
 * rationale. In production the real {@code jakarta.servlet-api} jar replaces
 * this stub.
 */
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
public @interface HandlesTypes {

    /** The classes (interfaces, supertypes, annotations) the SCI handles. */
    Class<?>[] value();
}
